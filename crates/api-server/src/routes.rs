//! API route handlers.
//!
//! All routes are defined under `/v1/` with utoipa annotations for OpenAPI
//! spec generation. Legacy unversioned paths (`/sessions`, etc.) redirect to
//! their `/v1/` equivalents with a 301 redirect for one release cycle.

use axum::{
    extract::{Path, State},
    http::{StatusCode, Uri},
    response::{Redirect, Sse},
    routing::{get, post},
    Json, Router,
};
use concerto_api_types::api::{
    CreateSessionRequest, CreateTaskRequest, SessionResponse, SpendSummaryResponse, TaskResponse,
};
use concerto_core::ids::Ulid;
use concerto_core::types::TaskId;
use concerto_core::CancellationToken;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::auth;
use crate::state::AppState;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn;
use std::env;

/// Cancels the contained [`CancellationToken`] when the request future is
/// dropped (e.g. on client disconnect). Handlers pass the token clone to
/// long-running store operations so they can stop promptly.
#[derive(Clone)]
struct RequestCancel {
    token: CancellationToken,
}

impl RequestCancel {
    fn new() -> (Self, RequestCancelGuard) {
        let token = CancellationToken::new();
        let guard = RequestCancelGuard(token.clone());
        (Self { token }, guard)
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

struct RequestCancelGuard(CancellationToken);

impl Drop for RequestCancelGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Build the router with /v1/ routes + legacy redirects + API key auth.
pub fn router(state: AppState) -> Router {
    // Swagger UI (CONCERTO_API_DOCS=1) registers /v1/openapi.json itself;
    // registering the manual route too would overlap and panic at startup.
    let docs_enabled = env::var("CONCERTO_API_DOCS").ok().as_deref() == Some("1");

    // /v1/ sub-router with all versioned routes, auth middleware, and body limit
    let mut v1_router = Router::new()
        .route("/health", get(health_check))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/tasks", post(create_task))
        .route("/sessions/{id}/tasks/{tid}/stream", get(stream_task_events))
        .route("/sessions/{id}/spend", get(get_spend_summary));
    if !docs_enabled {
        v1_router = v1_router.route("/openapi.json", get(serve_openapi));
    }
    let v1_router = v1_router
        .with_state(state.clone())
        .layer(from_fn(auth::auth_layer))
        .layer(DefaultBodyLimit::max(1_048_576));

    // Legacy unversioned routes → 301 redirect to /v1/
    let legacy_routes = Router::new()
        .route("/sessions", get(redirect_to_v1))
        .route("/sessions/{id}", get(redirect_to_v1))
        .route("/sessions/{id}/tasks", post(redirect_to_v1))
        .route("/sessions/{id}/tasks/{tid}/stream", get(redirect_to_v1))
        .route("/sessions/{id}/spend", get(redirect_to_v1));

    // Base router with optional Swagger UI based on env var
    let mut router = Router::new().nest("/v1", v1_router).merge(legacy_routes);

    if docs_enabled {
        router =
            router.merge(SwaggerUi::new("/v1/docs").url("/v1/openapi.json", ApiDoc::openapi()));
    }

    router
}

/// 301 redirect from a legacy unversioned path to its /v1/ equivalent.
async fn redirect_to_v1(uri: Uri) -> Redirect {
    let v1_path = format!("/v1{}", uri.path());
    Redirect::permanent(&v1_path)
}

// ---- Health & OpenAPI -------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/health",
    responses(
        (status = 200, description = "Server is healthy", content_type = "application/json"),
    ),
)]
/// GET /v1/health
async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Serve the generated OpenAPI spec.
async fn serve_openapi() -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let json_str = ApiDoc::openapi().to_json().map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("OpenAPI serialization failed: {e}"))
    })?;
    let value: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("OpenAPI JSON parse failed: {e}"))
    })?;
    Ok(Json(value))
}

// ---- Sessions ---------------------------------------------------------------

/// Validates that `project_dir` is a non-empty, existing, UTF-8 directory path.
///
/// When `project_roots` (the ADR-44 allowlist) is non-empty, additionally
/// rejects any canonicalized path that does not fall inside one of the
/// canonicalized roots with 403, so remote callers holding the API key cannot
/// root a session at an arbitrary filesystem location. Empty roots (the
/// default) keep behavior permissive.
fn validate_project_dir(
    project_dir: &str,
    project_roots: &[camino::Utf8PathBuf],
) -> Result<camino::Utf8PathBuf, (StatusCode, String)> {
    if project_dir.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "project_dir is required".into()));
    }

    let raw = camino::Utf8PathBuf::from(project_dir);

    let canonical = raw
        .canonicalize()
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid project_dir: {e}")))?;

    if !canonical.is_dir() {
        return Err((StatusCode::BAD_REQUEST, "project_dir must be a directory".into()));
    }

    let canonical = camino::Utf8PathBuf::from_path_buf(canonical)
        .map_err(|_| (StatusCode::BAD_REQUEST, "project_dir must be valid UTF-8".into()))?;

    // ADR-44 §2: hard out-of-root confinement when an allowlist is configured.
    // Both sides are canonicalized, so symlinks and `..` traversal are resolved
    // before the prefix check and cannot escape a configured root.
    if !project_roots.is_empty() && !confined_to_a_root(&canonical, project_roots) {
        return Err((
            StatusCode::FORBIDDEN,
            "project_dir is outside the allowed project roots".into(),
        ));
    }

    Ok(canonical)
}

/// Returns `true` when `path` is inside (or equal to) any configured root.
///
/// Each root is canonicalized once per call; a root that cannot be
/// canonicalized (missing or not a directory) matches nothing and is logged as
/// a warning rather than failing the request — the allowlist is configured
/// data, not a hard startup constraint.
fn confined_to_a_root(path: &camino::Utf8Path, project_roots: &[camino::Utf8PathBuf]) -> bool {
    for root in project_roots {
        let Ok(canonical_root) = root.canonicalize() else {
            tracing::warn!("configured project root is not a valid directory, ignoring: {root}");
            continue;
        };
        // A root that is not valid UTF-8 cannot be a prefix of the (UTF-8)
        // candidate path, so it matches nothing.
        let Ok(canonical_root) = camino::Utf8PathBuf::from_path_buf(canonical_root) else {
            continue;
        };
        if path.strip_prefix(&canonical_root).is_ok() {
            return true;
        }
    }
    false
}

#[utoipa::path(
    get,
    path = "/v1/sessions",
    responses(
        (status = 200, description = "List of recent sessions", body = Vec<SessionResponse>),
    ),
)]
/// GET /v1/sessions
async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<Vec<SessionResponse>>, (StatusCode, String)> {
    let (cancel, _guard) = RequestCancel::new();
    let sessions = state
        .store
        .list_recent_sessions(50, cancel.token())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let responses: Vec<SessionResponse> = sessions
        .into_iter()
        .map(|s| SessionResponse {
            id: s.id,
            created_at: s.created_at.to_string(),
            last_activity: s.created_at.to_string(),
            provider: s.provider,
            model: s.model,
        })
        .collect();

    Ok(Json(responses))
}

#[utoipa::path(
    post,
    path = "/v1/sessions",
    request_body = CreateSessionRequest,
    responses(
        (status = 200, description = "Session created", body = SessionResponse),
        (status = 403, description = "project_dir is outside the configured project roots"),
    ),
)]
/// POST /v1/sessions
async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<SessionResponse>, (StatusCode, String)> {
    let (cancel, _guard) = RequestCancel::new();
    let project_dir = validate_project_dir(&req.project_dir, &state.project_roots)?;
    let session = state
        .store
        .create_session(&project_dir, &req.provider, &req.model, cancel.token())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(SessionResponse {
        id: session.id,
        created_at: session.created_at.to_string(),
        last_activity: session.created_at.to_string(),
        provider: session.provider,
        model: session.model,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/sessions/{id}",
    params(
        ("id" = String, Path, description = "Session ULID"),
    ),
    responses(
        (status = 200, description = "Session details", body = SessionResponse),
        (status = 404, description = "Session not found"),
    ),
)]
/// GET /v1/sessions/{id}
async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<Ulid>,
) -> Result<Json<SessionResponse>, (StatusCode, String)> {
    let (cancel, _guard) = RequestCancel::new();
    let session = state
        .store
        .load_session(id, cancel.token())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("session {id} not found")))?;

    Ok(Json(SessionResponse {
        id: session.id,
        created_at: session.created_at.to_string(),
        last_activity: session.created_at.to_string(),
        provider: session.provider,
        model: session.model,
    }))
}

#[utoipa::path(
    post,
    path = "/v1/sessions/{id}/tasks",
    params(
        ("id" = String, Path, description = "Session ULID"),
    ),
    request_body = CreateTaskRequest,
    responses(
        (status = 200, description = "Task created", body = TaskResponse),
    ),
)]
/// POST /v1/sessions/{id}/tasks
async fn create_task(
    State(state): State<AppState>,
    Path(session_id): Path<Ulid>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<TaskResponse>, (StatusCode, String)> {
    let (cancel, _guard) = RequestCancel::new();
    let task = concerto_core::types::AgentTask {
        id: TaskId::new(),
        session_id,
        description: req.description,
        created_at: time::OffsetDateTime::now_utc(),
        execution_mode: Default::default(),
    };
    state
        .store
        .create_task(&task, cancel.token())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(TaskResponse {
        task_id: task.id,
        session_id: task.session_id,
        status: "pending".into(),
    }))
}

#[utoipa::path(
    get,
    path = "/v1/sessions/{id}/tasks/{tid}/stream",
    params(
        ("id" = String, Path, description = "Session ULID"),
        ("tid" = String, Path, description = "Task ID"),
    ),
    responses(
        (status = 200, description = "SSE event stream", content_type = "text/event-stream"),
        (status = 404, description = "Task not found under the given session"),
    ),
)]
/// GET /v1/sessions/{id}/tasks/{tid}/stream
async fn stream_task_events(
    State(state): State<AppState>,
    Path((session_id, task_id)): Path<(Ulid, TaskId)>,
) -> Result<
    Sse<
        impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    (StatusCode, String),
> {
    let (cancel, _guard) = RequestCancel::new();
    state
        .store
        .get_task(task_id, cancel.token())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .filter(|task| task.session_id == session_id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("task {task_id} not found")))?;

    let stream = crate::sse::SseAdapter::from_bus(&state.bus, task_id);
    Ok(Sse::new(stream))
}

#[utoipa::path(
    get,
    path = "/v1/sessions/{id}/spend",
    params(
        ("id" = String, Path, description = "Session ULID"),
    ),
    responses(
        (status = 200, description = "Spend summary", body = SpendSummaryResponse),
    ),
)]
/// GET /v1/sessions/{id}/spend
async fn get_spend_summary(
    State(state): State<AppState>,
    Path(id): Path<Ulid>,
) -> Result<Json<SpendSummaryResponse>, (StatusCode, String)> {
    let (cancel, _guard) = RequestCancel::new();
    let summary = state
        .store
        .spend_summary(id, cancel.token())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(SpendSummaryResponse {
        session_id: summary.session_id,
        total_usd: summary.total_cost_usd,
        total_tokens_in: summary.total_tokens_in,
        total_tokens_out: summary.total_tokens_out,
    }))
}

// ---- OpenAPI doc structure --------------------------------------------------

#[derive(OpenApi)]
#[openapi(
    paths(
        health_check,
        list_sessions,
        create_session,
        get_session,
        create_task,
        stream_task_events,
        get_spend_summary,
    ),
    components(
        schemas(
            concerto_api_types::api::SessionResponse,
            concerto_api_types::api::CreateSessionRequest,
            concerto_api_types::api::TaskResponse,
            concerto_api_types::api::CreateTaskRequest,
            concerto_api_types::api::SpendSummaryResponse,
        ),
    ),
    tags(
        (name = "sessions", description = "Session management"),
        (name = "system", description = "System health and metadata"),
    ),
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::CONCERTO_API_KEY_LOCK;
    use crate::state::AppState;
    use axum::{
        extract::{Path, State},
        http::StatusCode,
        response::IntoResponse,
        Json,
    };
    use concerto_api_types::api::{CreateSessionRequest, CreateTaskRequest};
    use concerto_core::event::EventBus;
    use concerto_core::ids::Ulid;
    use concerto_core::transcript::TranscriptEntry;
    use concerto_core::types::{AgentTask, Message, ProviderMetrics, TaskId};
    use concerto_core::CancellationToken;
    use concerto_sessions::replay::StoredEvent;
    use concerto_sessions::spend::{SpendRecord, SpendSummary};
    use concerto_sessions::{Session, SessionError, SessionStore, SessionSummary};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tower::ServiceExt;

    // ------------------------------------------------------------------
    // MockSessionStore — in-memory implementation for handler tests
    // ------------------------------------------------------------------

    #[derive(Clone)]
    struct MockSessionStore {
        sessions: Arc<Mutex<HashMap<Ulid, Session>>>,
        tasks: Arc<Mutex<HashMap<TaskId, AgentTask>>>,
    }

    impl MockSessionStore {
        fn new() -> Self {
            Self {
                sessions: Arc::new(Mutex::new(HashMap::new())),
                tasks: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionStore for MockSessionStore {
        async fn create_session(
            &self,
            project_dir: &camino::Utf8Path,
            provider: &str,
            model: &str,
            _cancel: CancellationToken,
        ) -> Result<Session, SessionError> {
            let session = Session {
                id: Ulid::new(),
                created_at: time::OffsetDateTime::now_utc(),
                project_dir: project_dir.to_path_buf(),
                provider: provider.to_string(),
                model: model.to_string(),
                total_tokens_in: 0,
                total_tokens_out: 0,
                total_cost_usd: 0.0,
            };
            self.sessions.lock().unwrap().insert(session.id, session.clone());
            Ok(session)
        }

        async fn load_session(
            &self,
            id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Option<Session>, SessionError> {
            Ok(self.sessions.lock().unwrap().get(&id).cloned())
        }

        async fn save_message(
            &self,
            _session_id: Ulid,
            _msg: &Message,
            _tokens_in: u64,
            _tokens_out: u64,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn append_messages(
            &self,
            _session_id: Ulid,
            _messages: &[Message],
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn load_messages(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<Message>, SessionError> {
            Ok(Vec::new())
        }

        async fn list_recent_sessions(
            &self,
            _limit: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<SessionSummary>, SessionError> {
            let sessions = self.sessions.lock().unwrap();
            Ok(sessions
                .values()
                .map(|s| SessionSummary {
                    id: s.id,
                    created_at: s.created_at,
                    provider: s.provider.clone(),
                    model: s.model.clone(),
                    message_count: 0,
                    total_cost_usd: s.total_cost_usd,
                    total_tokens_in: s.total_tokens_in,
                    total_tokens_out: s.total_tokens_out,
                })
                .collect())
        }

        async fn list_sessions_older_than(
            &self,
            _before_unix: i64,
            _cancel: CancellationToken,
        ) -> Result<Vec<SessionSummary>, SessionError> {
            Ok(Vec::new())
        }

        async fn delete_session(
            &self,
            _id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<bool, SessionError> {
            Ok(false)
        }

        async fn active_session_ids(
            &self,
            _cancel: CancellationToken,
        ) -> Result<Vec<Ulid>, SessionError> {
            Ok(Vec::new())
        }

        async fn list_sessions_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _limit: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<SessionSummary>, SessionError> {
            Ok(Vec::new())
        }

        async fn get_active_session_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _cancel: CancellationToken,
        ) -> Result<Option<Ulid>, SessionError> {
            Ok(None)
        }

        async fn set_active_session_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn record_metrics(
            &self,
            _session_id: Ulid,
            _metrics: ProviderMetrics,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn record_event(
            &self,
            _session_id: Ulid,
            _event: &concerto_core::event::Event,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn load_events(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<StoredEvent>, SessionError> {
            Ok(Vec::new())
        }

        async fn load_events_until(
            &self,
            _session_id: Ulid,
            _max_seq: u64,
            _cancel: CancellationToken,
        ) -> Result<Vec<StoredEvent>, SessionError> {
            Ok(Vec::new())
        }

        async fn record_spend(
            &self,
            _record: SpendRecord,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn list_spend_records(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<SpendRecord>, SessionError> {
            Ok(Vec::new())
        }

        async fn spend_summary(
            &self,
            session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<SpendSummary, SessionError> {
            let sessions = self.sessions.lock().unwrap();
            if let Some(s) = sessions.get(&session_id) {
                Ok(SpendSummary {
                    session_id,
                    total_cost_usd: s.total_cost_usd,
                    total_tokens_in: s.total_tokens_in,
                    total_tokens_out: s.total_tokens_out,
                    record_count: 0,
                })
            } else {
                Err(SessionError::NotFound(session_id.to_string()))
            }
        }

        async fn create_task(
            &self,
            task: &AgentTask,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            self.tasks.lock().unwrap().insert(task.id, task.clone());
            Ok(())
        }

        async fn update_task_status(
            &self,
            _task_id: TaskId,
            _status: &str,
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn get_task(
            &self,
            task_id: TaskId,
            _cancel: CancellationToken,
        ) -> Result<Option<AgentTask>, SessionError> {
            Ok(self.tasks.lock().unwrap().get(&task_id).cloned())
        }

        async fn list_tasks(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<AgentTask>, SessionError> {
            Ok(Vec::new())
        }

        async fn create_checkpoint(
            &self,
            _session_id: Ulid,
            _task_id: TaskId,
            _label: &str,
            _vfs_snapshot: &str,
            _sequence_num: u64,
            _cancel: CancellationToken,
        ) -> Result<Ulid, SessionError> {
            Ok(Ulid::new())
        }

        async fn load_checkpoint(
            &self,
            _checkpoint_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<(String, u64), SessionError> {
            Err(SessionError::NotFound("no checkpoints in mock".into()))
        }

        async fn list_checkpoints(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::CheckpointSummary>, SessionError> {
            Ok(Vec::new())
        }

        async fn save_orchestration_checkpoint(
            &self,
            _record: &concerto_sessions::OrchestrationCheckpointRecord,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn load_orchestration_checkpoint(
            &self,
            _session_id: Ulid,
        ) -> Result<Option<concerto_sessions::OrchestrationCheckpointRecord>, SessionError>
        {
            Ok(None)
        }

        async fn clear_orchestration_checkpoint(
            &self,
            _session_id: Ulid,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn append_transcript(
            &self,
            _session_id: Ulid,
            _entries: &[TranscriptEntry],
            _cancel: CancellationToken,
        ) -> Result<(), SessionError> {
            Ok(())
        }

        async fn load_transcript(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<TranscriptEntry>, SessionError> {
            Ok(Vec::new())
        }
    }

    // ------------------------------------------------------------------
    // Helper to build a test AppState with a mock store
    // ------------------------------------------------------------------

    fn test_state() -> AppState {
        AppState {
            bus: EventBus::default(),
            store: Arc::new(MockSessionStore::new()),
            project_roots: Vec::new(),
        }
    }

    fn test_state_with_store(store: MockSessionStore) -> AppState {
        AppState { bus: EventBus::default(), store: Arc::new(store), project_roots: Vec::new() }
    }

    /// Build a test state with a configured project-root allowlist.
    fn test_state_with_roots(
        store: MockSessionStore,
        project_roots: Vec<camino::Utf8PathBuf>,
    ) -> AppState {
        AppState { bus: EventBus::default(), store: Arc::new(store), project_roots }
    }

    // ==================================================================
    // Handler tests — call route functions directly
    // ==================================================================

    /// GET /v1/health returns status 200 with `{"status":"ok"}`.
    #[tokio::test]
    async fn health_check_returns_200_ok() {
        let result = health_check().await;
        assert_eq!(result.0.get("status").and_then(|v| v.as_str()), Some("ok"));
    }

    /// GET /v1/openapi.json returns a valid JSON OpenAPI document with
    /// required top-level fields.
    #[tokio::test]
    async fn serve_openapi_returns_valid_json() {
        let result = serve_openapi().await;
        assert!(result.is_ok());
        let json = result.unwrap();
        assert!(json.0.get("openapi").is_some(), "OpenAPI version field missing");
        assert!(json.0.get("info").is_some(), "OpenAPI info field missing");
        assert!(json.0.get("paths").is_some(), "OpenAPI paths field missing");
    }

    /// GET /v1/sessions returns an empty list when no sessions exist.
    #[tokio::test]
    async fn list_sessions_with_empty_store() {
        let state = test_state();
        let result = list_sessions(State(state)).await;
        assert!(result.is_ok());
        let sessions = result.unwrap();
        assert!(sessions.0.is_empty());
    }

    /// POST /v1/sessions with a valid request creates a new session.
    #[tokio::test]
    async fn create_session_with_valid_request() {
        let tmp = TempDir::new().unwrap();
        let state = test_state();
        let req = CreateSessionRequest {
            provider: "test-provider".into(),
            model: "test-model".into(),
            project_dir: tmp.path().to_str().unwrap().to_string(),
        };
        let result = create_session(State(state), Json(req)).await;
        assert!(result.is_ok());
        let session = result.unwrap();
        assert_eq!(session.0.provider, "test-provider");
        assert_eq!(session.0.model, "test-model");
    }

    /// POST /v1/sessions with an empty project_dir returns 400.
    #[tokio::test]
    async fn create_session_with_invalid_project_dir() {
        let state = test_state();
        let req = CreateSessionRequest {
            provider: "test".into(),
            model: "test".into(),
            project_dir: String::new(),
        };
        let result = create_session(State(state), Json(req)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    /// GET /v1/sessions/{id} returns the session when a valid ID is given.
    #[tokio::test]
    async fn get_session_with_valid_id() {
        let store = MockSessionStore::new();
        let state = test_state_with_store(store.clone());

        // Create a session via the handler first
        let tmp = TempDir::new().unwrap();
        let create_req = CreateSessionRequest {
            provider: "p1".into(),
            model: "m1".into(),
            project_dir: tmp.path().to_str().unwrap().to_string(),
        };
        let create_result = create_session(State(state.clone()), Json(create_req)).await;
        assert!(create_result.is_ok());
        let session_id = create_result.unwrap().0.id;

        // Now retrieve it
        let result = get_session(State(state), Path(session_id)).await;
        assert!(result.is_ok());
        let session = result.unwrap();
        assert_eq!(session.0.id, session_id);
        assert_eq!(session.0.provider, "p1");
    }

    /// GET /v1/sessions/{id} returns 404 when the session does not exist.
    #[tokio::test]
    async fn get_session_with_invalid_id_returns_404() {
        let state = test_state();
        let unknown_id = Ulid::new();
        let result = get_session(State(state), Path(unknown_id)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::NOT_FOUND);
    }

    /// POST /v1/sessions/{id}/tasks with a valid request creates a task.
    #[tokio::test]
    async fn create_task_with_valid_request() {
        let store = MockSessionStore::new();
        let state = test_state_with_store(store.clone());

        // Create a session first
        let tmp = TempDir::new().unwrap();
        let create_req = CreateSessionRequest {
            provider: "p1".into(),
            model: "m1".into(),
            project_dir: tmp.path().to_str().unwrap().to_string(),
        };
        let create_result = create_session(State(state.clone()), Json(create_req)).await;
        assert!(create_result.is_ok());
        let session_id = create_result.unwrap().0.id;

        // Now create a task
        let task_req = CreateTaskRequest { description: "Test task".into() };
        let result = create_task(State(state), Path(session_id), Json(task_req)).await;
        assert!(result.is_ok());
        let task = result.unwrap();
        assert_eq!(task.0.session_id, session_id);
        assert_eq!(task.0.status, "pending");
    }

    /// POST /v1/sessions/{id}/tasks with a valid session ID succeeds
    /// (the handler does not validate session existence before creating
    /// the task; the store call succeeds).
    #[tokio::test]
    async fn create_task_with_valid_session_id() {
        let state = test_state();
        let random_id = Ulid::new();
        let task_req = CreateTaskRequest { description: "Some task".into() };
        let result = create_task(State(state), Path(random_id), Json(task_req)).await;
        // The mock store does not check session existence, so this succeeds
        assert!(result.is_ok());
    }

    // ==================================================================
    // GET /v1/sessions/{id}/tasks/{tid}/stream tests
    // ==================================================================

    /// Helper: seed a session + one task via the handlers and return both IDs.
    async fn seed_session_and_task(store: MockSessionStore) -> (Ulid, TaskId) {
        let state = test_state_with_store(store);
        let tmp = TempDir::new().unwrap();
        let create_req = CreateSessionRequest {
            provider: "p1".into(),
            model: "m1".into(),
            project_dir: tmp.path().to_str().unwrap().to_string(),
        };
        let session_id = create_session(State(state.clone()), Json(create_req)).await.unwrap().0.id;
        let task_req = CreateTaskRequest { description: "Stream test task".into() };
        let task =
            create_task(State(state.clone()), Path(session_id), Json(task_req)).await.unwrap().0;
        (session_id, task.task_id)
    }

    /// Streaming a task under its correct session returns 200 (SSE stream opened).
    #[test]
    fn stream_task_events_under_correct_session_returns_200() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONCERTO_API_KEY");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let status = rt.block_on(async {
            let store = MockSessionStore::new();
            let (session_id, task_id) = seed_session_and_task(store.clone()).await;
            let app = router(test_state_with_store(store));
            let uri = format!("/v1/sessions/{session_id}/tasks/{task_id}/stream");
            app.oneshot(
                axum::http::Request::builder().uri(&uri).body(axum::body::Body::empty()).unwrap(),
            )
            .await
            .unwrap()
            .status()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(status, StatusCode::OK);
    }

    /// Streaming a task under a different, existing session's id returns 404 and no stream.
    #[test]
    fn stream_task_events_with_different_existing_session_returns_404() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONCERTO_API_KEY");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let status = rt.block_on(async {
            let store = MockSessionStore::new();
            let (session_id, task_id) = seed_session_and_task(store.clone()).await;
            let state = test_state_with_store(store);
            // Create a second, distinct session.
            let tmp = TempDir::new().unwrap();
            let create_req = CreateSessionRequest {
                provider: "p2".into(),
                model: "m2".into(),
                project_dir: tmp.path().to_str().unwrap().to_string(),
            };
            let other_session_id =
                create_session(State(state.clone()), Json(create_req)).await.unwrap().0.id;
            assert_ne!(other_session_id, session_id);

            let app = router(state);
            let uri = format!("/v1/sessions/{other_session_id}/tasks/{task_id}/stream");
            app.oneshot(
                axum::http::Request::builder().uri(&uri).body(axum::body::Body::empty()).unwrap(),
            )
            .await
            .unwrap()
            .status()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Streaming a task with a session id that does not exist returns 404.
    #[test]
    fn stream_task_events_with_nonexistent_session_returns_404() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONCERTO_API_KEY");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let status = rt.block_on(async {
            let store = MockSessionStore::new();
            let (session_id, task_id) = seed_session_and_task(store.clone()).await;
            let app = router(test_state_with_store(store));
            let unknown_id = Ulid::new();
            assert_ne!(unknown_id, session_id);
            let uri = format!("/v1/sessions/{unknown_id}/tasks/{task_id}/stream");
            app.oneshot(
                axum::http::Request::builder().uri(&uri).body(axum::body::Body::empty()).unwrap(),
            )
            .await
            .unwrap()
            .status()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// Streaming a nonexistent task id returns 404.
    #[test]
    fn stream_task_events_with_nonexistent_task_returns_404() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONCERTO_API_KEY");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let status = rt.block_on(async {
            let store = MockSessionStore::new();
            let (session_id, _task_id) = seed_session_and_task(store.clone()).await;
            let app = router(test_state_with_store(store));
            let unknown_task_id = TaskId::new();
            let uri = format!("/v1/sessions/{session_id}/tasks/{unknown_task_id}/stream");
            app.oneshot(
                axum::http::Request::builder().uri(&uri).body(axum::body::Body::empty()).unwrap(),
            )
            .await
            .unwrap()
            .status()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// GET /v1/sessions/{id}/spend returns the spend summary for a valid session.
    #[tokio::test]
    async fn get_spend_summary_with_valid_session() {
        let store = MockSessionStore::new();
        let state = test_state_with_store(store.clone());

        // Create a session first
        let tmp = TempDir::new().unwrap();
        let create_req = CreateSessionRequest {
            provider: "p1".into(),
            model: "m1".into(),
            project_dir: tmp.path().to_str().unwrap().to_string(),
        };
        let create_result = create_session(State(state.clone()), Json(create_req)).await;
        assert!(create_result.is_ok());
        let session_id = create_result.unwrap().0.id;

        // Get spend summary
        let result = get_spend_summary(State(state), Path(session_id)).await;
        assert!(result.is_ok());
        let summary = result.unwrap();
        assert_eq!(summary.0.session_id, session_id);
        assert_eq!(summary.0.total_usd, 0.0);
    }

    /// GET /v1/sessions/{id}/spend returns 500 when the session does not
    /// exist (the mock returns a `NotFound` error which the handler maps
    /// to INTERNAL_SERVER_ERROR).
    #[tokio::test]
    async fn get_spend_summary_with_invalid_session() {
        let state = test_state();
        let unknown_id = Ulid::new();
        let result = get_spend_summary(State(state), Path(unknown_id)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Legacy `/sessions` redirects to `/v1/sessions` with a 308
    /// (axum's `Redirect::permanent` uses 308 Permanent Redirect).
    #[tokio::test]
    async fn redirect_to_v1_for_sessions_path() {
        let uri: Uri = "/sessions".parse().unwrap();
        let redirect = redirect_to_v1(uri).await;
        let response = redirect.into_response();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(response.headers().get("location").unwrap(), "/v1/sessions",);
    }

    /// Legacy `/sessions/{id}` redirects to `/v1/sessions/{id}` with a 308.
    #[tokio::test]
    async fn redirect_to_v1_for_session_detail() {
        let uri: Uri = "/sessions/01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
        let redirect = redirect_to_v1(uri).await;
        let response = redirect.into_response();
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers().get("location").unwrap(),
            "/v1/sessions/01ARZ3NDEKTSV4RRFFQ69G5FAV",
        );
    }

    // ==================================================================
    // Router construction tests — verify the full Router is built
    // ==================================================================

    /// Global lock for `CONCERTO_API_DOCS` env var to prevent test races.
    static DOCS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Building the Router without `CONCERTO_API_DOCS` does NOT include
    /// the Swagger UI endpoint (checked at `/v1/docs/`).
    #[tokio::test]
    async fn router_construction_with_swagger_ui_disabled() {
        let future = {
            let _lock = DOCS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var("CONCERTO_API_DOCS");

            // Creating router under lock — router reads env at construction
            let app = router(test_state());
            app.oneshot(
                axum::http::Request::builder()
                    .uri("/v1/docs/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
        }; // lock dropped before await
        let response = future.await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Building the Router with `CONCERTO_API_DOCS=1` must succeed: the manual
    /// `/v1/openapi.json` route is skipped so `utoipa_swagger_ui`'s `.url()`
    /// handler does not overlap (previously this panicked at startup).
    #[test]
    fn router_construction_with_swagger_ui_enabled() {
        let _lock = DOCS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CONCERTO_API_DOCS", "1");

        let state = test_state();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _app = router(state);
        }));

        std::env::remove_var("CONCERTO_API_DOCS");
        assert!(
            result.is_ok(),
            "router() must not panic when CONCERTO_API_DOCS=1: Swagger UI \
             owns /v1/openapi.json and the manual route is skipped",
        );
    }

    // ==================================================================
    // Existing validate_project_dir tests preserved from original
    // ==================================================================

    #[test]
    fn validate_project_dir_rejects_missing() {
        let err = validate_project_dir("", &[]).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("required"));
    }

    #[test]
    fn validate_project_dir_rejects_blank() {
        let err = validate_project_dir("   ", &[]).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("required"));
    }

    #[test]
    fn validate_project_dir_rejects_non_existent() {
        let err = validate_project_dir("/tmp/does_not_exist_xyz", &[]).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_project_dir_rejects_file_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("test.txt");
        std::fs::write(&file_path, "data").unwrap();
        let err = validate_project_dir(file_path.to_str().unwrap(), &[]).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("directory"));
    }

    #[test]
    fn validate_project_dir_accepts_valid_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = validate_project_dir(tmp.path().to_str().unwrap(), &[]).unwrap();
        assert!(result.as_str().starts_with('/'));
        assert!(result.is_dir());
    }

    #[test]
    fn validate_project_dir_canonicalizes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let raw = tmp.path().to_str().unwrap().to_string();
        let result = validate_project_dir(&raw, &[]).unwrap();
        assert!(!result.as_str().contains(".."));
    }

    // ==================================================================
    // ADR-44 §2 project-root confinement tests
    // ==================================================================

    /// A session root outside every configured root is refused with 403 and no
    /// session is created.
    #[tokio::test]
    async fn create_session_rejects_out_of_root() {
        let store = MockSessionStore::new();
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let state = test_state_with_roots(store.clone(), vec![utf8_path_buf(root.path())]);

        let req = CreateSessionRequest {
            provider: "p".into(),
            model: "m".into(),
            project_dir: outside.path().to_str().unwrap().to_string(),
        };
        let result = create_session(State(state), Json(req)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::FORBIDDEN);
        assert!(store.sessions.lock().unwrap().is_empty(), "no session may be created out of root");
    }

    /// A session root nested inside a configured root is accepted.
    #[tokio::test]
    async fn create_session_accepts_in_root_subdir() {
        let root = tempfile::TempDir::new().unwrap();
        let sub = root.path().join("nested");
        std::fs::create_dir(&sub).unwrap();
        let state =
            test_state_with_roots(MockSessionStore::new(), vec![utf8_path_buf(root.path())]);

        let req = CreateSessionRequest {
            provider: "p".into(),
            model: "m".into(),
            project_dir: sub.to_str().unwrap().to_string(),
        };
        let result = create_session(State(state), Json(req)).await;
        assert!(result.is_ok());
        let session = result.unwrap();
        assert_eq!(session.0.provider, "p");
    }

    /// A session root exactly equal to a configured root is accepted.
    #[tokio::test]
    async fn create_session_accepts_root_equal_to_configured_root() {
        let root = tempfile::TempDir::new().unwrap();
        let state =
            test_state_with_roots(MockSessionStore::new(), vec![utf8_path_buf(root.path())]);

        let req = CreateSessionRequest {
            provider: "p".into(),
            model: "m".into(),
            project_dir: root.path().to_str().unwrap().to_string(),
        };
        let result = create_session(State(state), Json(req)).await;
        assert!(result.is_ok());
        let session = result.unwrap();
        assert_eq!(session.0.provider, "p");
    }

    /// A sibling directory sharing a string prefix with a configured root is
    /// refused: /tmp/root-a is not a root for /tmp/root-b (component-wise
    /// prefix match, not a string prefix match).
    #[tokio::test]
    async fn create_session_rejects_prefix_sibling_of_root() {
        let root = tempfile::TempDir::new().unwrap();
        let sibling = tempfile::TempDir::new().unwrap();
        assert!(!sibling.path().starts_with(root.path()), "sibling must not be inside the root");
        let state =
            test_state_with_roots(MockSessionStore::new(), vec![utf8_path_buf(root.path())]);

        let req = CreateSessionRequest {
            provider: "p".into(),
            model: "m".into(),
            project_dir: sibling.path().to_str().unwrap().to_string(),
        };
        let result = create_session(State(state), Json(req)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::FORBIDDEN);
    }

    /// Symlink and `..` traversal attempts cannot escape a configured root:
    /// both sides are canonicalized before the prefix comparison.
    #[cfg(unix)]
    #[tokio::test]
    async fn create_session_rejects_symlink_and_dotdot_traversal() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let link_path = root.path().join("escape-link");
        symlink(outside.path(), &link_path).unwrap();

        let state =
            test_state_with_roots(MockSessionStore::new(), vec![utf8_path_buf(root.path())]);

        // Via a symlink sitting inside the root but pointing outside it.
        let req = CreateSessionRequest {
            provider: "p".into(),
            model: "m".into(),
            project_dir: link_path.to_str().unwrap().to_string(),
        };
        let result = create_session(State(state.clone()), Json(req)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::FORBIDDEN);

        // Via a `..` traversal from inside the root targeting the outside dir.
        let outside_name = outside.path().file_name().unwrap();
        let traversal = root.path().join("..").join(outside_name);
        let req = CreateSessionRequest {
            provider: "p".into(),
            model: "m".into(),
            project_dir: traversal.to_str().unwrap().to_string(),
        };
        let result = create_session(State(state), Json(req)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::FORBIDDEN);
    }

    /// A missing project_dir still returns 400 when roots are configured
    /// (existing validation semantics are unchanged).
    #[tokio::test]
    async fn create_session_rejects_missing_path_with_roots() {
        let root = tempfile::TempDir::new().unwrap();
        let state =
            test_state_with_roots(MockSessionStore::new(), vec![utf8_path_buf(root.path())]);

        let req = CreateSessionRequest {
            provider: "p".into(),
            model: "m".into(),
            project_dir: "/tmp/definitely_not_a_real_dir_xyz".into(),
        };
        let result = create_session(State(state), Json(req)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    /// Helper: build a `Utf8PathBuf` from a `Path` (test tempdir paths are
    /// UTF-8).
    fn utf8_path_buf(path: &std::path::Path) -> camino::Utf8PathBuf {
        camino::Utf8PathBuf::from_path_buf(path.to_path_buf())
            .unwrap_or_else(|_| panic!("tempdir path is not UTF-8: {path:?}"))
    }
}
