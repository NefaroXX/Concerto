//! Persistent-context wrapper around the shared runtime runner.
//!
//! All frontends continue to call this module. Before dispatch, it replaces the
//! full persisted transcript with durable checkpoint summaries plus a recent
//! tail. The underlying runner still owns provider, tool, memory, and session
//! execution; after success we checkpoint any newly eligible history.

use concerto_core::CancellationToken;
use std::sync::Arc;

use camino::Utf8PathBuf;
use concerto_core::ids::Ulid;
use concerto_core::types::AgentOutput;
use concerto_core::OrchestratorError;

use crate::context_engine::ContextEngine;
use crate::session_manager::{ProjectSessionManager, SessionManagerConfig};

pub use crate::runtime_runner_impl::{
    init_memory_system, memory_enabled, ActiveMemoryServices, AgentRunRequest, SharedServices,
};

pub async fn run_shared_agent(
    mut request: AgentRunRequest,
    mut services: SharedServices,
) -> Result<AgentOutput, OrchestratorError> {
    let manager = match services.session_manager.clone() {
        Some(manager) => manager,
        None => {
            let config = SessionManagerConfig {
                git_auto_init: services
                    .config
                    .tool_settings
                    .as_ref()
                    .map(|settings| settings.git_auto_init)
                    .unwrap_or(true),
            };
            Arc::new(ProjectSessionManager::connect_with_config(config).await.map_err(|error| {
                OrchestratorError::AgentLoopError(format!("session store unavailable: {error}"))
            })?)
        }
    };
    services.session_manager = Some(manager.clone());
    let store = manager.store();

    let cancel = request.cancel_token.clone();
    let context_config = services.config.context.clone();
    let bus = services.bus.clone();
    let known_session = match request.session_id {
        Some(session_id) => Some(session_id),
        None => active_session_id(&request.project_dir, store.as_ref(), cancel.clone()).await,
    };
    if let Some(session_id) = known_session {
        let engine = ContextEngine::from_config(context_config.as_ref());
        match engine
            .assemble(
                store.clone(),
                session_id,
                &request.conversation_history,
                cancel.clone(),
                Some(&bus),
            )
            .await
        {
            Ok(history) => request.conversation_history = history,
            Err(error) => tracing::warn!(
                %error,
                %session_id,
                "failed to materialize durable context checkpoints; using supplied history"
            ),
        }
    }

    let output = crate::runtime_runner_impl::run_shared_agent(request, services).await?;
    let engine = ContextEngine::from_config(context_config.as_ref());
    if let Err(error) = engine.maintain(store, output.session_id, cancel, Some(&bus)).await {
        tracing::warn!(
            %error,
            session_id = %output.session_id,
            "failed to persist post-run context checkpoint"
        );
    }
    Ok(output)
}

async fn active_session_id(
    project_dir: &std::path::Path,
    store: &dyn concerto_sessions::SessionStore,
    cancel: CancellationToken,
) -> Option<Ulid> {
    let project = match Utf8PathBuf::from_path_buf(project_dir.to_path_buf()) {
        Ok(project) => project,
        Err(path) => {
            tracing::warn!(
                path = %path.display(),
                "cannot resolve active session for non-UTF-8 project path"
            );
            return None;
        }
    };
    match store.get_active_session_for_project(&project, cancel).await {
        Ok(session_id) => session_id,
        Err(error) => {
            tracing::warn!(%error, project = %project, "failed to resolve active project session");
            None
        }
    }
}
