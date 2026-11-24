//! H39b step 3: parent-owned supervised child history/metrics wiring.

use super::*;
use concerto_core::types::{Message, ProviderMetrics, Role, ToolCall, ToolResult};
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
    let history = SupervisedHistoryRun {
        directory: directory.path().to_path_buf(),
        paths,
        seeds: vec![SupervisedHistorySeed {
            agent_id: "reviewer".to_owned(),
            initial_seed_len: 2,
            paths: reviewer_paths.clone(),
        }],
    };

    let commands = build_supervised_commands(
        Path::new("/bin/true"),
        directory.path(),
        &tasks,
        &[("CONCERTO_PROVIDER".to_owned(), "mock".to_owned())],
        None,
        Some(&history),
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
    assert_eq!(
        reviewer.get(crate::supervised_history::SEED_LEN_ENV).map(String::as_str),
        Some("2")
    );
    assert_eq!(reviewer.get("CONCERTO_AGENT_ID").map(String::as_str), Some("reviewer"));

    let worker = envs(&commands[1].1);
    assert!(!worker.contains_key(crate::supervised_history::HISTORY_ENV));
    assert!(!worker.contains_key(crate::supervised_history::METRICS_ENV));
    assert!(!worker.contains_key(crate::supervised_history::SEED_LEN_ENV));

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
        agent_id: None,
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

/// R2: the seed is repaired before it is written and the stamped length is the
/// repaired length, so harvest cannot re-ingest a synthetic result.
#[tokio::test]
async fn prepare_repairs_seed_and_stamps_the_repaired_length() {
    let directory = tempfile::tempdir().expect("temp dir");
    let db_path = directory.path().join("sessions.db");
    let store = SqliteSessionStore::connect_path(&db_path).await.expect("file-backed store");
    let session_id = store
        .create_session(
            camino::Utf8Path::new("/tmp/h40-r2-prepare"),
            "mock",
            "mock",
            CancellationToken::new(),
        )
        .await
        .expect("session")
        .id;
    let store: Arc<dyn SessionStore> = Arc::new(store);

    let mut assistant = message(Role::Assistant, "");
    assistant.tool_calls = Some(vec![
        ToolCall {
            id: "call-a".to_owned(),
            name: "filesystem".to_owned(),
            arguments: serde_json::json!({ "path": "a.txt" }),
            thought_signature: None,
        },
        ToolCall {
            id: "call-b".to_owned(),
            name: "filesystem".to_owned(),
            arguments: serde_json::json!({ "path": "b.txt" }),
            thought_signature: None,
        },
    ]);
    let mut answered = message(Role::Tool, "");
    answered.tool_results = Some(vec![ToolResult {
        id: "call-a".to_owned(),
        name: "filesystem".to_owned(),
        content: serde_json::json!({ "ok": true }),
    }]);
    let raw = vec![message(Role::User, "go"), assistant, answered];
    store
        .append_agent_messages(session_id, "reviewer", &raw, CancellationToken::new())
        .await
        .expect("seed raw history");

    let tasks = vec![("reviewer".to_owned(), "review".to_owned())];
    let prepared = prepare_supervised_history(Some(&store), session_id, &tasks)
        .await
        .expect("prepare history");
    assert_eq!(prepared.seeds.len(), 1);
    assert_eq!(
        prepared.seeds[0].initial_seed_len, 4,
        "the stamp is the repaired length, not the raw length"
    );

    let written = crate::supervised_history::read_messages(&prepared.seeds[0].paths.history)
        .await
        .expect("read repaired seed");
    assert_eq!(written.len(), 4);
    assert_eq!(
        written[2].tool_results.as_ref().map(|results| results[0].id.as_str()),
        Some("call-a"),
        "the recorded result is preserved"
    );
    assert_eq!(
        written[3].tool_results.as_ref().map(|results| results[0].id.as_str()),
        Some("call-b"),
        "the unanswered call gets a request-only result"
    );
    assert_eq!(
        written[3].tool_results.as_ref().map(|results| &results[0].content["status"]),
        Some(&serde_json::json!("outcome_unknown"))
    );

    let persisted = store
        .load_agent_messages(session_id, "reviewer", CancellationToken::new())
        .await
        .expect("load raw history");
    assert_eq!(persisted.len(), 3, "the synthetic result is never persisted");
    assert_eq!(
        persisted[2].tool_results.as_ref().map(Vec::len),
        Some(1),
        "only the genuinely recorded result is durable"
    );
    assert_eq!(
        persisted[2].tool_results.as_ref().map(|results| results[0].id.as_str()),
        Some("call-a")
    );

    let _ = tokio::fs::remove_dir_all(&prepared.directory).await;
}

/// Regression for the H39b harvest offset-loss bug and R2: startup must never
/// rewrite the accumulated file, and the parent's stamped seed length must be
/// the repaired length so harvest slices only child turns.
#[tokio::test]
async fn startup_never_rewrites_a_repaired_seed_and_harvest_keeps_every_child_turn() {
    let directory = tempfile::tempdir().expect("temp dir");
    let db_path = directory.path().join("sessions.db");
    let store = SqliteSessionStore::connect_path(&db_path).await.expect("file-backed store");
    let session_id = store
        .create_session(
            camino::Utf8Path::new("/tmp/h39b-offset"),
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
    unanswered.tool_calls = Some(vec![ToolCall {
        id: "seed-call".to_owned(),
        name: "filesystem".to_owned(),
        arguments: serde_json::json!({ "path": "seed.txt" }),
        thought_signature: None,
    }]);
    let raw_seed = vec![message(Role::User, "seed question"), unanswered];
    let seed = crate::provider_transcript::restore_tool_responses(raw_seed);
    assert_eq!(seed.len(), 3, "the parent repairs the seed before writing");
    let seed_len = seed.len();
    std::fs::write(&paths.history, serde_json::to_vec(&seed).expect("serialize seed"))
        .expect("write seed fixture");
    let before = std::fs::read(&paths.history).expect("read seed bytes");

    // Child startup: repair is idempotent for the already-repaired seed, and
    // the file must stay byte-identical so the harvest offset remains exact.
    let restored = crate::supervised_history::restore_for_child(&paths.history)
        .await
        .expect("restore child history");
    assert_eq!(
        serde_json::to_value(&restored).expect("serialize restored seed"),
        serde_json::to_value(&seed).expect("serialize written seed"),
        "the repaired seed is unchanged for the loop"
    );
    assert_eq!(
        std::fs::read(&paths.history).expect("reread seed bytes"),
        before,
        "child startup never rewrites the accumulated file"
    );

    // Child delta follows the seed and is appended without trimming.
    let delta =
        vec![seeded_message(Role::User, "child task"), seeded_message(Role::Assistant, "done")];
    crate::supervised_history::append_turns(&paths.history, &delta)
        .await
        .expect("simulate child append");

    let seed = SupervisedHistorySeed {
        agent_id: "reviewer".to_owned(),
        initial_seed_len: seed_len,
        paths: paths.clone(),
    };
    harvest_supervised_history(Some(&store), session_id, None, &[seed]).await;

    let accumulated = store
        .load_agent_messages(session_id, "reviewer", CancellationToken::new())
        .await
        .expect("load agent history");
    assert_eq!(
        accumulated.len(),
        delta.len(),
        "no offset loss: every child turn is persisted: {accumulated:?}"
    );
    assert_eq!(accumulated[0].content, "child task");
    assert_eq!(accumulated[1].content, "done");
    assert!(
        !accumulated.iter().any(|message| message.content == "seed question"),
        "no seed turn is persisted as child history"
    );
}

/// R1 regression: two distinct, valid configured ids that sanitize to the
/// same readable prefix must get separate files, histories, and attributed
/// spend.
#[tokio::test]
async fn colliding_agent_ids_get_separate_paths_histories_and_metrics() {
    let directory = tempfile::tempdir().expect("temp dir");
    let db_path = directory.path().join("sessions.db");
    let store = SqliteSessionStore::connect_path(&db_path).await.expect("file-backed store");
    let session_id = store
        .create_session(
            camino::Utf8Path::new("/tmp/h40-r1"),
            "mock",
            "mock",
            CancellationToken::new(),
        )
        .await
        .expect("session")
        .id;
    let store: Arc<dyn SessionStore> = Arc::new(store);

    let dotted_id = "review.v1";
    let underscored_id = "review_v1";
    let dotted_seed = vec![
        seeded_message(Role::User, "dotted question"),
        seeded_message(Role::Assistant, "dotted answer"),
    ];
    let underscored_seed = vec![
        seeded_message(Role::User, "underscore question"),
        seeded_message(Role::Assistant, "underscore answer"),
    ];
    store
        .append_agent_messages(session_id, dotted_id, &dotted_seed, CancellationToken::new())
        .await
        .expect("seed dotted agent history");
    store
        .append_agent_messages(
            session_id,
            underscored_id,
            &underscored_seed,
            CancellationToken::new(),
        )
        .await
        .expect("seed underscored agent history");

    let tasks = vec![
        (dotted_id.to_owned(), "dotted task".to_owned()),
        (underscored_id.to_owned(), "underscore task".to_owned()),
    ];
    let prepared = prepare_supervised_history(Some(&store), session_id, &tasks)
        .await
        .expect("prepare history");
    assert_eq!(prepared.seeds.len(), 2, "both distinct agents keep history");
    let dotted_paths = prepared.paths.get(dotted_id).expect("dotted paths").clone();
    let underscored_paths = prepared.paths.get(underscored_id).expect("underscored paths").clone();
    assert_ne!(dotted_paths.history, underscored_paths.history);
    assert_ne!(dotted_paths.metrics, underscored_paths.metrics);

    // Each child restores only its own seed.
    let dotted_restored =
        crate::supervised_history::read_messages(&dotted_paths.history).await.expect("dotted seed");
    let underscored_restored = crate::supervised_history::read_messages(&underscored_paths.history)
        .await
        .expect("underscore seed");
    assert_eq!(
        dotted_restored.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
        vec!["dotted question", "dotted answer"]
    );
    assert_eq!(
        underscored_restored.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
        vec!["underscore question", "underscore answer"]
    );

    // Each child appends to its own file.
    let dotted_delta = vec![
        seeded_message(Role::User, "dotted child task"),
        seeded_message(Role::Assistant, "dotted done"),
    ];
    crate::supervised_history::append_turns(&dotted_paths.history, &dotted_delta)
        .await
        .expect("append dotted turns");
    let underscored_delta = vec![
        seeded_message(Role::User, "underscore child task"),
        seeded_message(Role::Assistant, "underscore done"),
    ];
    crate::supervised_history::append_turns(&underscored_paths.history, &underscored_delta)
        .await
        .expect("append underscore turns");

    // Distinct metric lines stay on their own sidecars.
    let dotted_metric = ProviderMetrics {
        agent_id: None,
        provider: "dotted-provider".to_owned(),
        model: "mock-model".to_owned(),
        tokens_in: 11,
        tokens_out: 5,
        cost_usd: 0.02,
        latency_ms: 3,
    };
    let underscored_metric = ProviderMetrics {
        agent_id: None,
        provider: "underscore-provider".to_owned(),
        model: "mock-model".to_owned(),
        tokens_in: 22,
        tokens_out: 9,
        cost_usd: 0.04,
        latency_ms: 4,
    };
    crate::supervised_history::append_metrics(&dotted_paths.metrics, &[dotted_metric])
        .await
        .expect("append dotted metrics");
    crate::supervised_history::append_metrics(&underscored_paths.metrics, &[underscored_metric])
        .await
        .expect("append underscore metrics");

    let root_task = Ulid::new();
    harvest_supervised_history(Some(&store), session_id, Some(root_task), &prepared.seeds).await;

    let dotted_stream = store
        .load_agent_messages(session_id, dotted_id, CancellationToken::new())
        .await
        .expect("load dotted history");
    let underscored_stream = store
        .load_agent_messages(session_id, underscored_id, CancellationToken::new())
        .await
        .expect("load underscore history");
    assert_eq!(
        dotted_stream.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
        vec!["dotted question", "dotted answer", "dotted child task", "dotted done"]
    );
    assert_eq!(
        underscored_stream.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
        vec![
            "underscore question",
            "underscore answer",
            "underscore child task",
            "underscore done",
        ]
    );

    let spend =
        store.list_spend_records(session_id, CancellationToken::new()).await.expect("list spend");
    assert_eq!(spend.len(), 2, "each agent's spend is recorded exactly once");
    let dotted_spend = spend
        .iter()
        .find(|record| record.agent_id.as_deref() == Some(dotted_id))
        .expect("dotted spend");
    assert_eq!(dotted_spend.provider, "dotted-provider");
    assert_eq!((dotted_spend.tokens_in, dotted_spend.tokens_out), (11, 5));
    let underscored_spend = spend
        .iter()
        .find(|record| record.agent_id.as_deref() == Some(underscored_id))
        .expect("underscore spend");
    assert_eq!(underscored_spend.provider, "underscore-provider");
    assert_eq!((underscored_spend.tokens_in, underscored_spend.tokens_out), (22, 9));

    // Authoritative metric rows are separate, not merged under one identity.
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path);
    let pool = sqlx::SqlitePool::connect_with(options).await.expect("metrics pool");
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT provider, tokens_in FROM provider_metrics WHERE session_id = ? ORDER BY provider",
    )
    .bind(session_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("query provider metrics");
    pool.close().await;
    assert_eq!(
        rows,
        vec![("dotted-provider".to_owned(), 11), ("underscore-provider".to_owned(), 22)]
    );

    let _ = tokio::fs::remove_dir_all(&prepared.directory).await;
}

/// R1: duplicate physical paths disable history before any child is spawned.
#[tokio::test]
async fn duplicate_physical_paths_disable_history_before_spawn() {
    let tasks = vec![
        ("reviewer".to_owned(), "first".to_owned()),
        ("reviewer".to_owned(), "second".to_owned()),
    ];
    let prepared =
        prepare_supervised_history(None, Ulid::new(), &tasks).await.expect("prepare history");
    assert!(prepared.paths.is_empty(), "a shared path must not be handed to a child");
    assert!(prepared.seeds.is_empty(), "no seed may be written to a shared path");
    let _ = tokio::fs::remove_dir_all(&prepared.directory).await;
}

#[tokio::test]
async fn harvest_persists_an_interrupted_tail_for_load_time_repair() {
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
    assert_eq!(accumulated.len(), 1, "the interrupted call is durable evidence");
    assert_eq!(
        accumulated[0].tool_calls.as_ref().map(|calls| calls[0].id.as_str()),
        Some("call-1"),
        "harvest keeps the raw assistant call"
    );

    // Load-time repair supplies the missing result for the next request
    // without persisting a fabricated result into the agent stream.
    let repaired = crate::provider_transcript::restore_tool_responses(accumulated);
    assert_eq!(repaired.len(), 2);
    let synthetic = repaired[1]
        .tool_results
        .as_ref()
        .and_then(|results| results.first())
        .expect("synthetic result for the interrupted call");
    assert_eq!(synthetic.id, "call-1");
    assert_eq!(synthetic.content["status"], "outcome_unknown");
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
        agent_id: None,
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
