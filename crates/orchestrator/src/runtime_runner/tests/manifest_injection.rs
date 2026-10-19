//! Single-agent environment-manifest prompt injection (ADR-82 parity slice).
//!
//! The single-agent run collects its manifest through the SAME barrier the
//! coordinator path uses ([`run_environment_manifest_barrier`]) and injects
//! the digest through the SAME block shape
//! ([`crate::environment_manifest::format_manifest_block`]). These tests pin
//! the integration: one shared cache, one probe set per run even with
//! `environment_describe` registered, and the digest in the single-agent
//! system prompt.

use super::super::*;
use super::stage_harness::TestAudit;
use crate::environment_describe::EnvironmentDescribeTool;
use crate::environment_manifest::CountingProber;
use concerto_core::traits::tool::Tool as _;

/// Minimal stub tool so the run inventory is non-trivial.
struct StubTool;
#[async_trait::async_trait]
impl concerto_core::traits::tool::Tool for StubTool {
    fn name(&self) -> &str {
        "stub-tool"
    }
    fn description(&self) -> &str {
        "test stub"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn capability_requirements(&self) -> concerto_core::types::CapabilitySet {
        concerto_core::types::CapabilitySet::default()
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _policy: &dyn concerto_core::traits::policy::PolicyEngine,
        _session: &SessionContext,
        _cancel: CancellationToken,
    ) -> Result<concerto_core::types::ToolOutput, concerto_core::ToolError> {
        Ok(concerto_core::types::ToolOutput {
            summary: "stub".to_string(),
            data: serde_json::json!({}),
        })
    }
}

/// One probe set per run with the describe operation registered: the shared
/// barrier collects once, populates the shared cache, and every later
/// describe call serves the cache with zero further probes — while the
/// barrier digest renders the shared block in the single-agent prompt.
#[tokio::test]
async fn single_agent_barrier_collects_once_and_describe_serves_the_cache() {
    let project_dir = tempfile::tempdir().expect("tempdir for run");
    let project_path = project_dir.path().to_path_buf();
    let config = AppConfig::default();

    // One shared registry: the stub plus `environment_describe` registered
    // LAST (the production `setup_shared_tooling` order), so the tool-names
    // snapshot covers every tool.
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(StubTool));
    let describe_state = crate::environment_describe::register_describe_tool(
        &mut registry,
        &project_path,
        &config,
        false,
        false,
        None,
    );
    // Swap the production prober for the counting stub so the probe
    // discipline is pinned exactly (the barrier takes its prober as a
    // parameter — the same seam the collector tests use).
    let counting = std::sync::Arc::new(CountingProber::default());
    describe_state.lock().unwrap_or_else(|error| error.into_inner()).prober = counting.clone();

    let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
    let policy =
        std::sync::Arc::new(SimplePolicyEngine::new(allow_all, std::sync::Arc::new(TestAudit)));
    let executor = std::sync::Arc::new(ToolExecutor::new(std::sync::Arc::new(registry), policy));

    let session_id = Ulid::new();
    let manifest = run_environment_manifest_barrier(
        &executor,
        &config,
        &project_path,
        session_id,
        false,
        None,
        false,
        counting.as_ref(),
        &CancellationToken::new(),
        &describe_state,
    )
    .await
    .expect("the shared barrier collects a manifest for the single-agent run");
    assert_eq!(manifest.revision, 0, "a fresh run starts at revision 0");
    assert_eq!(
        manifest.executables.len(),
        crate::environment_manifest::DEFAULT_PROBE_CANDIDATES.len(),
        "one observation per probe candidate"
    );
    let barrier_probes = counting.calls();
    assert!(
        barrier_probes <= crate::environment_manifest::DEFAULT_PROBE_CANDIDATES.len(),
        "at most one probe per candidate in the single set: {barrier_probes}"
    );

    // The barrier digest renders the shared block in the single-agent system
    // prompt (requirement (a): block present when a manifest is collected).
    let digest = manifest.summary();
    let builder = PromptBuilder::new("System prompt".to_string())
        .with_environment_manifest(Some(digest.clone()));
    let request = builder.build("", &[], None, None);
    let system = &request.messages[0].content;
    let expected = crate::environment_manifest::format_manifest_block(Some(&digest))
        .expect("the digest renders a block");
    assert!(
        system.contains(&expected),
        "single-agent prompt must carry the shared block: {system}"
    );

    // The describe operation serves the barrier-populated cache with zero
    // further probes across mixed calls (requirement (c): one probe set per
    // run even with the describe operation registered — one cache, one
    // collection).
    let tool = EnvironmentDescribeTool::with_state(describe_state);
    let session = SessionContext::new(Ulid::new(), project_path);
    let policy = SimplePolicyEngine::new(vec![], std::sync::Arc::new(TestAudit));
    for input in [
        serde_json::json!({}),
        serde_json::json!({"detail": "full"}),
        serde_json::json!({"detail": "summary"}),
        serde_json::json!({}),
    ] {
        let output = tool
            .execute(input, &policy, &session, CancellationToken::new())
            .await
            .expect("describe serves the barrier manifest");
        assert_eq!(output.data["revision"], 0, "the cached revision is served unchanged");
    }
    assert_eq!(
        counting.calls(),
        barrier_probes,
        "no per-call re-probe: describe served the shared cache"
    );
}

/// Fail-soft at the wiring level: a run with no barrier manifest (digest
/// `None`) prompts exactly as before — no block, no error text.
#[test]
fn single_agent_prompt_without_a_barrier_manifest_has_no_block() {
    let builder = PromptBuilder::new("System prompt".to_string()).with_environment_manifest(None);
    let request = builder.build("", &[], None, None);
    let system = &request.messages[0].content;
    assert!(!system.contains("<environment_manifest>"), "no block without a manifest: {system}");
    assert!(!system.contains("error"), "no error text without a manifest: {system}");
    assert!(system.contains("## Environment"), "the identity card is intact: {system}");
}
