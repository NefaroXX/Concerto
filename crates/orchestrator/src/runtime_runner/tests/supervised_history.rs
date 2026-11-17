//! H39b step 3: parent-owned supervised child history/metrics wiring.

use super::*;
use concerto_core::types::{Message, ProviderMetrics, Role};
use concerto_sessions::SqliteSessionStore;

fn message(role: Role, content: &str) -> Message {
    Message {
        gemini_replay: None,
        role,
        content: content.to_owned(),
        tool_calls: None,
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
    }
}

fn seeded_message(role: Role, content: &str) -> Message {
    message(role, content)
}

fn envs(command: &std::process::Command) -> HashMap<String, String> {
    command
        .get_envs()
        .filter_map(|(name, value)| {
            Some((name.to_string_lossy().into_owned(), value?.to_string_lossy().into_owned()))
        })
        .collect()
}

#[test]
fn build_supervised_commands_stamps_history_paths_only_when_available() {
    let directory = tempfile::tempdir().expect("temp dir");
    let tasks = vec![
        ("reviewer".to_owned(), "review".to_owned()),
        ("worker".to_owned(), "work".to_owned()),
    ];
    let reviewer_paths = crate::supervised_history::paths_for(directory.path(), "reviewer");
    let mut paths = HashMap::new();
    paths.insert("reviewer".to_owned(), reviewer_paths.clone());

    let commands = build_supervised_commands(
        Path::new("/bin/true"),
        directory.path(),
        &tasks,
        &[("CONCERTO_PROVIDER".to_owned(), "mock".to_owned())],
        None,
        Some(&paths),
    );
    assert_eq!(commands.len(), 2);

    let reviewer = envs(&commands[0].1);
    assert_eq!(
        reviewer.get(crate::supervised_history::HISTORY_ENV),
        Some(&reviewer_paths.history.to_string_lossy().into_owned())
    );
    assert_eq!(
        reviewer.get(crate::supervised_history::METRICS_ENV),
        Some(&reviewer_paths.metrics.to_string_lossy().into_owned())
    );
    assert_eq!(reviewer.get("CONCERTO_AGENT_ID").map(String::as_str), Some("reviewer"));

    let worker = envs(&commands[1].1);
    assert!(!worker.contains_key(crate::supervised_history::HISTORY_ENV));
    assert!(!worker.contains_key(crate::supervised_history::METRICS_ENV));

    let no_history = build_supervised_commands(
        Path::new("/bin/true"),
        directory.path(),
        &tasks,
        &[],
        None,
        None,
    );
    assert!(!envs(&no_history[0].1).contains_key(crate::supervised_history::HISTORY_ENV));
}

#[tokio::test]
async fn prepare_and_harvest_restore_append_and_attribute_child_history() {
    let directory = tempfile::tempdir().expect("temp dir");
    let db_path = directory.path().join("sessions.db");
    let store = SqliteSessionStore::connect_path(&db_path).await.expect("file-backed store");
    let session_id = store
        .create_session(
            camino::Utf8Path::new("/tmp/h39b-supervised"),
            "mock",
            "mock",
            CancellationToken::new(),
        )
        .await
        .expect("session")
        .id;
    let store: Arc<dyn SessionStore> = Arc::new(store);

    let seed = vec![
        seeded_message(Role::User, "prior question"),
        seeded_message(Role::Assistant, "prior answer"),
    ];
    store
        .append_agent_messages(session_id, "reviewer", &seed, CancellationToken::new())
        .await
        .expect("seed agent history");

    let tasks = vec![("reviewer".to_owned(), "new task".to_owned())];
    let prepared = prepare_supervised_history(Some(&store), session_id, &tasks)
        .await
        .expect("prepare history");
    let paths = prepared.paths.get("reviewer").expect("reviewer paths").clone();
    assert_eq!(prepared.seeds.len(), 1);
    assert_eq!(prepared.seeds[0].initial_seed_len, 2);

    let restored =
        crate::supervised_history::read_messages(&paths.history).await.expect("read seed");
    assert_eq!(restored.len(), 2, "seed restored without duplication");

    let delta =
        vec![seeded_message(Role::User, "new task"), seeded_message(Role::Assistant, "done")];
    crate::supervised_history::append_turns(&paths.history, &delta)
        .await
        .expect("simulate child append");
    let metric = ProviderMetrics {
        provider: "mock".to_owned(),
        model: "mock-model".to_owned(),
        tokens_in: 11,
        tokens_out: 7,
        cost_usd: 0.02,
        latency_ms: 3,
    };
    crate::supervised_history::append_metrics(&paths.metrics, &[metric])
        .await
        .expect("simulate child metrics");

    let root_task = Ulid::new();
    harvest_supervised_history(Some(&store), session_id, Some(root_task), &prepared.seeds).await;

    let accumulated = store
        .load_agent_messages(session_id, "reviewer", CancellationToken::new())
        .await
        .expect("load agent history");
    assert_eq!(accumulated.len(), 4, "seed plus only the child's new turns");
    assert_eq!(accumulated[0].content, "prior question");
    assert_eq!(accumulated[1].content, "prior answer");
    assert_eq!(accumulated[2].content, "new task");
    assert_eq!(accumulated[3].content, "done");

    let spend =
        store.list_spend_records(session_id, CancellationToken::new()).await.expect("list spend");
    assert_eq!(spend.len(), 1);
    assert_eq!(spend[0].agent_id.as_deref(), Some("reviewer"));
    assert_eq!(spend[0].task_id, Some(root_task));
    assert_eq!(spend[0].provider, "mock");
    assert_eq!((spend[0].tokens_in, spend[0].tokens_out), (11, 7));

    // The trait has no provider-metrics reader; open a second pool to assert
    // the authoritative row (and that harvest wrote it exactly once).
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path);
    let pool = sqlx::SqlitePool::connect_with(options).await.expect("metrics pool");
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT provider, COUNT(*) FROM provider_metrics WHERE session_id = ? GROUP BY provider",
    )
    .bind(session_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("query provider metrics");
    pool.close().await;
    assert_eq!(rows, vec![("mock".to_owned(), 1)], "exactly one child metric row");

    let _ = tokio::fs::remove_dir_all(&prepared.directory).await;
}

#[tokio::test]
async fn harvest_trims_a_trailing_unanswered_tool_call() {
    let directory = tempfile::tempdir().expect("temp dir");
    let db_path = directory.path().join("sessions.db");
    let store = SqliteSessionStore::connect_path(&db_path).await.expect("file-backed store");
    let session_id = store
        .create_session(
            camino::Utf8Path::new("/tmp/h39b-trim"),
            "mock",
            "mock",
            CancellationToken::new(),
        )
        .await
        .expect("session")
        .id;
    let store: Arc<dyn SessionStore> = Arc::new(store);

    let paths = crate::supervised_history::paths_for(directory.path(), "reviewer");
    let mut unanswered = message(Role::Assistant, "");
    unanswered.tool_calls = Some(vec![concerto_core::types::ToolCall {
        id: "call-1".to_owned(),
        name: "filesystem".to_owned(),
        arguments: serde_json::json!({ "path": "x" }),
        thought_signature: None,
    }]);
    let seed = SupervisedHistorySeed {
        agent_id: "reviewer".to_owned(),
        initial_seed_len: 0,
        paths: paths.clone(),
    };
    crate::supervised_history::write_seed(&paths.history, &[]).await.expect("empty seed");
    crate::supervised_history::append_turns(&paths.history, &[unanswered])
        .await
        .expect("append unresolved call");

    harvest_supervised_history(Some(&store), session_id, None, &[seed]).await;

    let accumulated = store
        .load_agent_messages(session_id, "reviewer", CancellationToken::new())
        .await
        .expect("load agent history");
    assert!(accumulated.is_empty(), "unanswered tool call is never persisted");
}

#[tokio::test]
async fn metrics_are_harvested_even_when_history_is_unreadable() {
    let directory = tempfile::tempdir().expect("temp dir");
    let db_path = directory.path().join("sessions.db");
    let store = SqliteSessionStore::connect_path(&db_path).await.expect("file-backed store");
    let session_id = store
        .create_session(
            camino::Utf8Path::new("/tmp/h39b-metrics-only"),
            "mock",
            "mock",
            CancellationToken::new(),
        )
        .await
        .expect("session")
        .id;
    let store: Arc<dyn SessionStore> = Arc::new(store);

    let paths = crate::supervised_history::paths_for(directory.path(), "reviewer");
    std::fs::write(&paths.history, b"not json").expect("corrupt history fixture");
    let metric = ProviderMetrics {
        provider: "mock".to_owned(),
        model: "mock-model".to_owned(),
        tokens_in: 5,
        tokens_out: 2,
        cost_usd: 0.01,
        latency_ms: 1,
    };
    crate::supervised_history::append_metrics(&paths.metrics, &[metric])
        .await
        .expect("child metrics fixture");

    let seed = SupervisedHistorySeed {
        agent_id: "reviewer".to_owned(),
        initial_seed_len: 2,
        paths: paths.clone(),
    };
    harvest_supervised_history(Some(&store), session_id, None, &[seed]).await;

    let messages = store
        .load_agent_messages(session_id, "reviewer", CancellationToken::new())
        .await
        .expect("load agent history");
    assert!(messages.is_empty(), "unreadable history is skipped, not partially written");
    let spend =
        store.list_spend_records(session_id, CancellationToken::new()).await.expect("list spend");
    assert_eq!(spend.len(), 1, "metrics survive an independent history read failure");
    assert_eq!(spend[0].agent_id.as_deref(), Some("reviewer"));
}
