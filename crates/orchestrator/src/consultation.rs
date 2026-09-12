//! Consultation machinery (issue #59, parent #51) — a typed CONSULT
//! operation alongside dispatch.
//!
//! `call_specialist` always means: dispatch a task that may mutate (SubTask
//! node, ownership, completion state). A CONSULT is different in kind: the
//! named specialist answers a bounded question in a READ-ONLY advisory
//! capacity and its findings return as structured evidence — never as task
//! results, never as graph nodes, never touching ownership/completion state.
//!
//! The read-only boundary binds at the execution seam
//! ([`ConsultReadOnlyPolicy`]): every tool call the consultant makes is
//! evaluated by this wrapper BEFORE the run's own policy — only explicit
//! filesystem read operations pass through to the run's rules, everything
//! else (filesystem writes, shell, git, MCP, unknown tools, and calls with a
//! missing operation) fails closed with a recorded denial. The wrapper is a
//! strict subset, never a widening: allowed reads still evaluate under the
//! run's policy engine unchanged.
//!
//! The consultant runs on a minimal toolset (the filesystem tool only —
//! reads) over a fresh registry ([`consult_read_only_executor`]), so the
//! consultant cannot even see mutating tools; combined with the policy gate
//! this is the "writes denied even under an Acting run grant" guarantee.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use concerto_core::error::PolicyError;
use concerto_core::executor::ToolExecutor;
use concerto_core::traits::policy::{AuditEntry, AuditLog, PolicyEngine};
use concerto_core::types::{PolicyAction, PolicyVerdict, ToolDefinition, ToolRegistry};
use concerto_core::CancellationToken;

/// The tool with which the Coordinator consults a registered specialist
/// (issue #59) — the typed CONSULT operation beside `call_specialist`.
pub(crate) const CONSULT_SPECIALIST_TOOL: &str = "consult_specialist";

/// Default consultation effort cap: the maximum tool executions one
/// consultation may perform (small by design — consults are advisory).
pub const DEFAULT_CONSULT_MAX_TOOL_CALLS: u32 = 8;

/// Hard ceiling on a model-supplied effort cap. Anything above it is a
/// rejection, not a silent bound (the model output is untrusted).
pub const MAX_CONSULT_MAX_TOOL_CALLS: u32 = 16;

/// Maximum characters accepted for a consultation question. Consults are
/// bounded advisory questions — tighter than the dispatch task bound.
pub const MAX_CONSULT_QUESTION_CHARS: usize = 2_000;

/// Maximum characters of the findings recorded as evidence.
pub const MAX_CONSULT_FINDINGS_CHARS: usize = 4_000;

/// The audit `rule_matched` label for a write-classified denial.
const RULE_CONSULT_READ_ONLY: &str = "consult_read_only";
/// The audit `rule_matched` label for an effort-cap denial.
const RULE_CONSULT_EFFORT_CAPPED: &str = "consult_effort_capped";

/// Argument schema for the Coordinator's `consult_specialist` tool.
pub(crate) fn consult_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CONSULT_SPECIALIST_TOOL.to_string(),
        description: "Consult a registered specialist for ADVICE. Read-only: the \
                      consultant cannot write files or mutate the workspace, and \
                      consultation never dispatches task work. Its findings are \
                      recorded as evidence (an event id you can cite in later \
                      decisions). Use it to resolve open questions or \
                      low-confidence decisions before dispatching work."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {
                    "type": "string",
                    "description": "The id of the registered specialist to consult, exactly as listed in the roster."
                },
                "question": {
                    "type": "string",
                    "description": "The complete, self-contained question for the consultant."
                },
                "notes": {
                    "type": "string",
                    "description": "Optional short context for the consultant (pointers, constraints)."
                },
                "supporting_evidence_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional real whiteboard event ids (from the context) that ground the question. Fabricated ids are rejected."
                },
                "max_tool_calls": {
                    "type": "integer",
                    "description": "Optional effort cap: the maximum read-only tool executions the consultant may perform (default small)."
                }
            },
            "required": ["agent_id", "question"]
        }),
    }
}

/// One `consult_specialist` tool call's parsed arguments.
pub(crate) struct ConsultSpecialistArgs {
    pub agent_id: String,
    pub question: String,
    pub notes: Option<String>,
    pub supporting_evidence_ids: Vec<String>,
    pub max_tool_calls: Option<u32>,
}

impl ConsultSpecialistArgs {
    /// Parse the tool arguments. Malformed arguments (missing or non-string
    /// `agent_id`/`question`) yield `None` — the caller answers with a
    /// structured tool error, never a crash.
    pub(crate) fn parse(arguments: &serde_json::Value) -> Option<Self> {
        let agent_id = arguments.get("agent_id").and_then(serde_json::Value::as_str)?;
        let question = arguments.get("question").and_then(serde_json::Value::as_str)?;
        let max_tool_calls = arguments
            .get("max_tool_calls")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        Some(Self {
            agent_id: agent_id.to_owned(),
            question: question.to_owned(),
            notes: arguments.get("notes").and_then(serde_json::Value::as_str).map(str::to_owned),
            supporting_evidence_ids: crate::coordinator::parse_string_array(
                arguments,
                "supporting_evidence_ids",
            ),
            max_tool_calls,
        })
    }
}

/// The consultant's task text: the read-only advisory framing plus the
/// question (and optional coordinator notes). The framing is part of the
/// consultation contract — the consultant is told its findings are evidence,
/// not task work.
pub(crate) fn consult_task_description(question: &str, notes: Option<&str>) -> String {
    let mut brief = format!(
        "CONSULTATION (read-only advisory — you cannot modify the workspace; no \
         task work is dispatched from this run; your findings are recorded as \
         evidence for the Coordinator):\n\n{question}"
    );
    if let Some(notes) = notes {
        brief.push_str(&format!("\n\nCoordinator notes: {notes}"));
    }
    brief
}

/// The consultation write classification (deterministic, fail-closed): only
/// explicit filesystem READ operations pass; everything else — filesystem
/// write/delete/move/copy, a missing or malformed operation, shell, git,
/// MCP tools, unknown tools — is a consultation write attempt.
pub(crate) fn is_consultation_read(action: &PolicyAction<'_>) -> bool {
    if action.tool_name != "filesystem" {
        return false;
    }
    matches!(
        action.input.get("operation").and_then(serde_json::Value::as_str),
        Some("read") | Some("list") | Some("exists")
    )
}

/// The read-only policy gate every consultation tool call evaluates under
/// (issue #59).
///
/// A wrapping [`PolicyEngine`], never a replacement: the run's own policy
/// engine is untouched and still decides every action this gate lets
/// through. Writes are denied EVEN when the run's policy would allow them
/// (e.g. an Acting run grant) — the allow-list is explicit filesystem reads
/// only, so the gate is a strict subset, never a widening. Every denial is
/// recorded through the run's audit log (fail-soft) with the denying rule
/// label, so a blocked mutation attempt is observable, never silent.
///
/// The gate also enforces the consultation effort cap: the allowed-read
/// budget is consumed per executed call; once exhausted, further reads are
/// denied with [`RULE_CONSULT_EFFORT_CAPPED`].
pub(crate) struct ConsultReadOnlyPolicy {
    inner: Arc<dyn PolicyEngine>,
    /// Remaining allowed tool executions for THIS consultation.
    remaining: AtomicU32,
}

impl ConsultReadOnlyPolicy {
    pub(crate) fn new(inner: Arc<dyn PolicyEngine>, max_tool_calls: u32) -> Self {
        Self { inner, remaining: AtomicU32::new(max_tool_calls.max(1)) }
    }

    /// Record one denial through the run's audit log (fail-soft — an audit
    /// failure never turns into a different tool outcome; the denial itself
    /// is returned regardless).
    async fn record_denial(
        &self,
        action: &PolicyAction<'_>,
        rule: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: action.tool_name.to_owned(),
            verdict: "Deny".to_owned(),
            input_hash: blake3::hash(action.input.to_string().as_bytes()).to_hex().to_string(),
            session_id: action.session_id,
            correlation_id: action.correlation_id,
            timestamp: time::OffsetDateTime::now_utc(),
            user_response: Some(format!(
                "consultation denied ({rule}): the consult runs under a read-only \
                 boundary and this action is not a filesystem read"
            )),
            rule_matched: Some(rule.to_owned()),
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
        };
        if let Err(error) = self.inner.audit_log().record(entry, cancel).await {
            tracing::warn!(
                %error,
                rule,
                "issue #59: consultation denial audit row failed to record (fail-soft)"
            );
        }
    }
}

#[async_trait::async_trait]
impl PolicyEngine for ConsultReadOnlyPolicy {
    async fn evaluate(
        &self,
        action: &PolicyAction<'_>,
        cancel: CancellationToken,
    ) -> Result<PolicyVerdict, PolicyError> {
        if !is_consultation_read(action) {
            self.record_denial(action, RULE_CONSULT_READ_ONLY, cancel.clone()).await;
            return Ok(PolicyVerdict::Deny);
        }
        // The effort cap is consumed per granted execution. `fetch_update`
        // fails exactly when the budget is already zero — the denial then
        // consumes nothing further.
        let granted = self
            .remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if !granted {
            self.record_denial(action, RULE_CONSULT_EFFORT_CAPPED, cancel.clone()).await;
            return Ok(PolicyVerdict::Deny);
        }
        // Allowed reads still evaluate under the run's own policy — the
        // strict-subset contract (never a widening).
        self.inner.evaluate(action, cancel).await
    }

    fn audit_log(&self) -> &dyn AuditLog {
        self.inner.audit_log()
    }
}

/// Build the consultation's read-only executor (issue #59): a minimal
/// toolset (the filesystem tool only) over a fresh registry, with every call
/// evaluated under [`ConsultReadOnlyPolicy`]. The consultant never shares
/// the run executor's write-capable grant — this facade IS the read-only
/// grant boundary, enforced at the execution seam (every tool call the
/// consultant makes evaluates here first; the run's policy engine decides
/// the reads that pass the gate, unchanged).
///
/// The tool is anchored to the session's project directory, so consultant
/// reads observe the same workspace the dispatched agents see.
pub(crate) fn consult_read_only_executor(
    project_dir: &std::path::Path,
    inner: Arc<dyn PolicyEngine>,
    max_tool_calls: u32,
) -> ToolExecutor {
    let mut registry = ToolRegistry::default();
    let root = camino::Utf8PathBuf::from_path_buf(project_dir.to_path_buf()).unwrap_or_default();
    registry.register(Box::new(concerto_tools::filesystem::FilesystemTool::new(root)));
    ToolExecutor::new(
        Arc::new(registry),
        Arc::new(ConsultReadOnlyPolicy::new(inner, max_tool_calls)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::traits::policy::AuditEntry;
    use concerto_core::types::Condition;
    use concerto_core::{policy::SimplePolicyEngine, types::PolicyRule};
    use std::sync::Mutex;

    struct ProbeAudit {
        entries: Mutex<Vec<AuditEntry>>,
    }

    impl ProbeAudit {
        fn new() -> Arc<Self> {
            Arc::new(Self { entries: Mutex::new(Vec::new()) })
        }

        fn rows(&self) -> Vec<AuditEntry> {
            self.entries.lock().unwrap_or_else(|error| error.into_inner()).clone()
        }
    }

    #[async_trait::async_trait]
    impl AuditLog for ProbeAudit {
        async fn record(
            &self,
            entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            self.entries.lock().unwrap_or_else(|error| error.into_inner()).push(entry);
            Ok(())
        }
    }

    fn action<'a>(tool_name: &'a str, input: &'a serde_json::Value) -> PolicyAction<'a> {
        PolicyAction {
            tool_name,
            input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: concerto_core::types::CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        }
    }

    fn allow_all() -> (Arc<SimplePolicyEngine>, Arc<ProbeAudit>) {
        let audit = ProbeAudit::new();
        let engine = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            audit.clone(),
        ));
        (engine, audit)
    }

    #[test]
    fn consultation_read_classification_is_read_only() {
        let read = serde_json::json!({ "operation": "read", "path": "a" });
        let list = serde_json::json!({ "operation": "list", "path": "." });
        let exists = serde_json::json!({ "operation": "exists", "path": "a" });
        assert!(is_consultation_read(&action("filesystem", &read)));
        assert!(is_consultation_read(&action("filesystem", &list)));
        assert!(is_consultation_read(&action("filesystem", &exists)));

        let write = serde_json::json!({ "operation": "write", "path": "a" });
        let delete = serde_json::json!({ "operation": "delete", "path": "a" });
        let move_op = serde_json::json!({ "operation": "move", "path": "a" });
        let copy = serde_json::json!({ "operation": "copy", "path": "a" });
        let no_op = serde_json::json!({ "path": "a" });
        let shell = serde_json::json!({ "command": "ls" });
        let git = serde_json::json!({ "operation": "status" });
        let mcp = serde_json::json!({});
        for (tool, input) in [
            ("filesystem", &write),
            ("filesystem", &delete),
            ("filesystem", &move_op),
            ("filesystem", &copy),
            ("filesystem", &no_op),
            ("shell", &shell),
            ("git", &git),
            ("mcp:server:tool", &mcp),
        ] {
            assert!(!is_consultation_read(&action(tool, input)), "{tool} must fail closed");
        }
    }

    #[tokio::test]
    async fn write_attempt_is_denied_and_recorded() {
        let (inner, audit) = allow_all();
        let gate = ConsultReadOnlyPolicy::new(inner, DEFAULT_CONSULT_MAX_TOOL_CALLS);
        let input = serde_json::json!({ "operation": "write", "path": "x.rs", "content": "!" });
        let verdict = gate.evaluate(&action("filesystem", &input), CancellationToken::new()).await;
        assert!(matches!(verdict, Ok(PolicyVerdict::Deny)), "writes deny even under allow-all");
        let rows = audit.rows();
        assert_eq!(rows.len(), 1, "the denial is recorded: {rows:?}");
        assert_eq!(rows[0].rule_matched.as_deref(), Some(RULE_CONSULT_READ_ONLY));
        assert_eq!(rows[0].verdict, "Deny");
        assert_eq!(rows[0].tool_name, "filesystem");
    }

    #[tokio::test]
    async fn allowed_reads_delegate_to_the_run_policy() {
        let (inner, audit) = allow_all();
        let gate = ConsultReadOnlyPolicy::new(inner, DEFAULT_CONSULT_MAX_TOOL_CALLS);
        let input = serde_json::json!({ "operation": "read", "path": "a.rs" });
        let verdict = gate.evaluate(&action("filesystem", &input), CancellationToken::new()).await;
        assert!(matches!(verdict, Ok(PolicyVerdict::Allow)), "reads follow the run's rules");
        let rows = audit.rows();
        assert!(
            rows.iter().any(|row| row.verdict == "Allow"),
            "the run policy decided the read: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.rule_matched.as_deref() == Some(RULE_CONSULT_READ_ONLY)),
            "an allowed read is not a consultation denial: {rows:?}"
        );
    }

    #[tokio::test]
    async fn effort_cap_denies_beyond_the_bound_and_records() {
        let (inner, audit) = allow_all();
        let gate = ConsultReadOnlyPolicy::new(inner, 2);
        let input = serde_json::json!({ "operation": "read", "path": "a.rs" });
        for _ in 0..2 {
            let verdict =
                gate.evaluate(&action("filesystem", &input), CancellationToken::new()).await;
            assert!(matches!(verdict, Ok(PolicyVerdict::Allow)), "reads within the cap run");
        }
        let verdict = gate.evaluate(&action("filesystem", &input), CancellationToken::new()).await;
        assert!(matches!(verdict, Ok(PolicyVerdict::Deny)), "the cap fails closed");
        let rows = audit.rows();
        assert!(
            rows.iter().any(|row| row.rule_matched.as_deref() == Some(RULE_CONSULT_EFFORT_CAPPED)),
            "the cap denial is recorded: {rows:?}"
        );
    }

    #[test]
    fn consult_args_parse_bounds_and_evidence() {
        let args = ConsultSpecialistArgs::parse(&serde_json::json!({
            "agent_id": "researcher",
            "question": "why?",
            "notes": "be brief",
            "supporting_evidence_ids": ["ev-1", 3, "ev-2"],
            "max_tool_calls": 4,
        }))
        .expect("parses");
        assert_eq!(args.agent_id, "researcher");
        assert_eq!(args.question, "why?");
        assert_eq!(args.notes.as_deref(), Some("be brief"));
        assert_eq!(args.supporting_evidence_ids, vec!["ev-1".to_owned(), "ev-2".to_owned()]);
        assert_eq!(args.max_tool_calls, Some(4));

        assert!(ConsultSpecialistArgs::parse(&serde_json::json!({ "agent_id": "a" })).is_none());
        assert!(ConsultSpecialistArgs::parse(&serde_json::json!({ "question": "?" })).is_none());
    }

    #[test]
    fn consult_task_description_frames_the_read_only_contract() {
        let brief = consult_task_description("why does it fail?", Some("check the graph"));
        assert!(brief.contains("read-only"), "the framing is part of the contract: {brief}");
        assert!(brief.contains("why does it fail?"));
        assert!(brief.contains("Coordinator notes: check the graph"));
        let bare = consult_task_description("why?", None);
        assert!(!bare.contains("Coordinator notes"));
    }

    #[test]
    fn consult_tool_definition_names_the_operation() {
        let definition = consult_tool_definition();
        assert_eq!(definition.name, CONSULT_SPECIALIST_TOOL);
        assert!(definition.description.contains("Read-only"));
    }
}
