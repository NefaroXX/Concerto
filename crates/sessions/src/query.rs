//! Pure render/query helpers for sessions evidence payloads (NORM S5).
//!
//! Single home of the **pure** whiteboard/resource-facts payload readers and
//! builders — fact rendering, payload building, row/payload projection. No
//! SQL, store, checkpoint, or migration code lives here ([`crate::whiteboard`]
//! and [`crate::resource_facts`]). Writer and reader of the shared payload
//! shapes sit side by side so a key rename is one edit with failing contract
//! tests, not a silent read-time fallback (issue #136). Every accessor is
//! total and additive — unknown keys degrade to `None` / an empty view and
//! the CALLER keeps its own explicit fallback (historical payloads keep
//! reading).

use std::collections::BTreeMap;

use crate::whiteboard::NewWhiteboardEvent;
use crate::SessionError;

// Shared payload keys (issue #136, #141) — writer/reader contract.

/// `Finding` payload key carrying finding text written by the consultative
/// writer ([`consult_finding_payload`]); `summary`/`content` stay accepted
/// aliases for pre-#136 or third-party Findings.
const FINDING_TEXT_KEY: &str = "findings";
const FINDING_SUMMARY_KEY: &str = "summary";
const FINDING_CONTENT_KEY: &str = "content";

/// `WriteApplied` payload key carrying the write tool's input object; the
/// written path is that object's `path` field (the write tools' argument).
const WRITE_INPUT_KEY: &str = "input";
/// The path field name shared by a tool input object and an observed-path
/// record (`ToolExecuted` payload rows).
const PATH_KEY: &str = "path";

/// The payload key carrying cited evidence ids: ADR-65 §6 fixes it on
/// `Decision` payloads, and the consultative `Finding` writer emits it as
/// the grounding refs (issue #141) the world model projects. One key, one
/// reader ([`supporting_evidence_ids`]) — writer and reader cannot drift.
const SUPPORTING_EVIDENCE_KEY: &str = "supporting_evidence_ids";

// Payload readers (total, additive) and builders.

/// Read the text a `Finding` label should carry: the consultative writer's
/// `findings` first (#136), then the legacy `summary`/`content` aliases.
/// `None` when no string text — the caller keeps its own fallback rather
/// than the fact being dropped.
#[must_use]
pub fn finding_text(payload: &serde_json::Value) -> Option<&str> {
    [FINDING_TEXT_KEY, FINDING_SUMMARY_KEY, FINDING_CONTENT_KEY]
        .iter()
        .find_map(|key| payload.get(*key).and_then(serde_json::Value::as_str))
}

/// Read the evidence ids a payload cites through [`SUPPORTING_EVIDENCE_KEY`]:
/// the optional array ADR-65 §6 fixes on `Decision` payloads and the
/// consultative `Finding` writer ([`consult_finding_payload`]) emits as an
/// assertion's grounding refs (issue #141). Total and additive like the
/// other accessors: absent key, non-array or non-string entries ⇒ those ids
/// are simply not cited (empty vec). The accessor validates NOTHING — the
/// append path checks ids for a `Decision` (ADR-65 §6); the world model
/// stores what it reads as provenance only.
#[must_use]
pub fn supporting_evidence_ids(payload: &serde_json::Value) -> Vec<String> {
    payload
        .get(SUPPORTING_EVIDENCE_KEY)
        .and_then(serde_json::Value::as_array)
        .map(|ids| ids.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Build the payload of the consultative `Finding` the coordinator appends
/// for a consultation (issue #59, #136): `{ consultative, question, findings,
/// supporting_evidence_ids, hypothesis_id }`. `question` is bounded by the
/// caller; the world model reads `findings` back via [`finding_text`].
#[must_use]
pub fn consult_finding_payload(
    question: &str,
    findings: &str,
    supporting_evidence_ids: &[String],
    hypothesis_id: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "consultative": true,
        "question": question,
        FINDING_TEXT_KEY: findings,
        SUPPORTING_EVIDENCE_KEY: supporting_evidence_ids,
        "hypothesis_id": hypothesis_id,
    })
}

/// Build the payload of the write gate's `WriteApplied` record (issue #136):
/// `{ tool, input, policy_verdict: "allow", pre_images }` plus, when the
/// request was canonicalized supervisor/in-process-side (ADR-82 slice 1), a
/// `registered_as` echo of the tool name the caller actually invoked. An
/// applied write is by definition allowed; [`write_applied_path`] reads the
/// path back from `input.path`, sharing the `input` key, and
/// [`write_applied_registered_as`] reads the echo back — writer and reader
/// cannot drift.
#[must_use]
pub fn write_applied_payload(
    tool: &str,
    input: &serde_json::Value,
    pre_images: &BTreeMap<String, String>,
    registered_as: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "tool": tool,
        WRITE_INPUT_KEY: input,
        "policy_verdict": "allow",
        "pre_images": pre_images,
    });
    if let Some(registered_as) = registered_as {
        payload["registered_as"] = serde_json::Value::String(registered_as.to_owned());
    }
    payload
}

/// Read the path a `WriteApplied` event wrote (`input.path`, the write gate's
/// shape) — the world model's verified write facts and artifact attribution
/// read this. `None` when the payload carries no path: the caller decides;
/// never fabricate a path.
#[must_use]
pub fn write_applied_path(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get(WRITE_INPUT_KEY)
        .and_then(|input| input.get(PATH_KEY))
        .and_then(serde_json::Value::as_str)
}

/// `WriteApplied` payload key echoing the **registered** tool name the caller
/// invoked, present only on rows whose request was canonicalized
/// (supervisor/in-process request assembly, ADR-82 slice 1): the `tool` key
/// carries the canonical policy-view name there, and this echo preserves the
/// requested identity for forensics.
const WRITE_REGISTERED_AS_KEY: &str = "registered_as";

/// Read the registered tool name a `WriteApplied` event was invoked under
/// (ADR-82 slice 1). `None` on rows written before canonicalization (their
/// `tool` key already IS the registered name) and on non-alias writes (the
/// two names agree); never inferred from `tool`.
#[must_use]
pub fn write_applied_registered_as(payload: &serde_json::Value) -> Option<&str> {
    payload.get(WRITE_REGISTERED_AS_KEY).and_then(serde_json::Value::as_str)
}

/// Extract the file paths touched by a `WriteApplied` payload, using the
/// same defensive grammar as `fold_ledger` in the orchestrator: the
/// `pre_images` map keys when present, else `path`/`target`/`input.path`.
/// Empty means the event carried no path info. Used by
/// [`crate::resource_facts::ResourceFacts::rebuild_from_log`] to dirty rows
/// whose events carry no project-root attribution.
pub(crate) fn write_applied_paths(payload: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(pre_images) = payload.get("pre_images").and_then(|v| v.as_object()) {
        paths.extend(pre_images.keys().cloned());
        return paths;
    }
    if let Some(path) = payload.get("path").and_then(|v| v.as_str()) {
        paths.push(path.to_owned());
    } else if let Some(target) = payload.get("target").and_then(|v| v.as_str()) {
        paths.push(target.to_owned());
    } else if let Some(input) = payload.get("input").and_then(|v| v.as_object()) {
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            paths.push(path.to_owned());
        }
    }
    paths
}

// ToolExecuted world-model view (W1 outcome classification).

/// How a `ToolExecuted` event's tool call ended (W1 outcome
/// classification): `ok` ran and succeeded, `failed` ran and reported
/// failure, `denied` is a policy/approval/capability refusal that never
/// executed, `interrupted` was cancelled or timed out. `Unknown` is the
/// read-side fallback for payloads that predate the `outcome` key (or
/// carry an unrecognized value) — it derives no fact and no failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Ok,
    Failed,
    Denied,
    Interrupted,
    Unknown,
}

impl ToolOutcome {
    /// The payload spelling the writer stamps (`"ok"`, `"failed"`,
    /// `"denied"`, `"interrupted"`; `Unknown` is never written).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        }
    }

    /// Classify a `ToolExecuted` payload: an explicit `outcome` key wins
    /// (unrecognized values read `Unknown`, never a guess); without one
    /// the legacy `success` flag decides (`true` → `Ok`, `false` →
    /// `Failed`); a missing `success` key reads `Unknown`.
    #[must_use]
    pub fn parse(outcome: Option<&str>, success: Option<bool>) -> Self {
        match outcome {
            Some("ok") => Self::Ok,
            Some("failed") => Self::Failed,
            Some("denied") => Self::Denied,
            Some("interrupted") => Self::Interrupted,
            Some(_) => Self::Unknown,
            None => match success {
                Some(true) => Self::Ok,
                Some(false) => Self::Failed,
                None => Self::Unknown,
            },
        }
    }
}

/// What the world model reads from a `ToolExecuted` payload (issue #136):
/// the tool name, its canonical arguments, the success flag, the outcome
/// class, the observed paths and the exit code — the fields its verified
/// tool facts and C-FAIL contradiction derive from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutedView<'a> {
    /// The tool name; `None` when the payload carries no usable name.
    pub tool: Option<&'a str>,
    /// The canonical arguments (`Value::Null` when the payload has none).
    pub args: &'a serde_json::Value,
    /// Whether the execution succeeded — only successes project to facts.
    pub success: bool,
    /// How the execution ended (W1): explicit `outcome` key, else the
    /// legacy `success` fallback (`true` → `Ok`, `false` → `Failed`,
    /// missing → `Unknown`).
    pub outcome: ToolOutcome,
    /// The observed paths, in payload order.
    pub paths: Vec<&'a str>,
    /// The numeric exit code the payload carried (W1 — the contradicting
    /// failure's structure; `None` when absent, null, non-numeric or out of
    /// `i32` range).
    pub exit_code: Option<i32>,
    /// The canonical policy-view tool name (ADR-82 slice 1): `filesystem`
    /// for a `write` alias; `None` on payloads recorded before the canonical
    /// keys existed.
    pub canonical_tool: Option<&'a str>,
    /// The canonical operation from the policy-view input (`Some("write")`
    /// for an alias write whose recorded args named no `operation` field);
    /// `None` on legacy payloads.
    pub canonical_operation: Option<&'a str>,
}

/// The `args` stand-in for a payload that carries none — a view sentinel
/// only, never written back to the log.
static NO_TOOL_ARGS: serde_json::Value = serde_json::Value::Null;

/// Read the world-model fields of a `ToolExecuted` payload. Total: any
/// payload shape yields a view (`success` is `false` unless the payload says
/// so, paths default empty), so an unreadable field degrades to "no fact" at
/// the caller instead of a panic or a fabricated observation.
#[must_use]
pub fn tool_executed_view(payload: &serde_json::Value) -> ToolExecutedView<'_> {
    let success = payload.get("success").and_then(serde_json::Value::as_bool);
    ToolExecutedView {
        tool: payload.get("tool").and_then(serde_json::Value::as_str),
        args: payload.get("args").unwrap_or(&NO_TOOL_ARGS),
        success: success == Some(true),
        outcome: ToolOutcome::parse(
            payload.get("outcome").and_then(serde_json::Value::as_str),
            success,
        ),
        exit_code: payload
            .get("exit_code")
            .and_then(serde_json::Value::as_i64)
            .and_then(|code| i32::try_from(code).ok()),
        canonical_tool: payload.get("canonical_tool").and_then(serde_json::Value::as_str),
        canonical_operation: payload.get("canonical_operation").and_then(serde_json::Value::as_str),
        paths: payload
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get(PATH_KEY).and_then(serde_json::Value::as_str))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

// Content hash (pure; the whiteboard append path uses this).

/// Deterministic blake3 fingerprint of a [`NewWhiteboardEvent`]'s canonical
/// content fields: `event_id`, `agent_id`, `kind` (kebab-case), `scope`,
/// `session_id`, `plan_id`, `causation`, `payload` (compact JSON — `Value`'s
/// map sorts keys, so equal values serialize equal), `pre_image_hash`, and
/// `created_at` (8-byte BE integer). Each field is length-prefixed (8-byte
/// big-endian length + raw bytes) so boundaries are unambiguous; `None`
/// optionals contribute empty strings.
///
/// `gate_seq`/`agent_seq` are **not** included: they are log-assigned
/// sequencing artifacts, not event content, so replay verification can
/// recompute the expected hash from caller-attested fields alone.
pub fn compute_content_hash(event: &NewWhiteboardEvent) -> Result<String, SessionError> {
    let payload_json = serde_json::to_string(&event.payload)?;
    // Capacity floor only: 10 canonical fields each contribute at least their
    // 8-byte length prefix; the buffer grows as needed.
    let mut buf: Vec<u8> = Vec::with_capacity(10 * 8);
    push_field(&mut buf, event.event_id.as_bytes());
    push_field(&mut buf, event.agent_id.as_bytes());
    push_field(&mut buf, event.kind.as_str().as_bytes());
    push_field(&mut buf, event.scope.as_bytes());
    push_field(&mut buf, event.session_id.as_deref().unwrap_or("").as_bytes());
    push_field(&mut buf, event.plan_id.as_deref().unwrap_or("").as_bytes());
    push_field(&mut buf, event.causation.as_deref().unwrap_or("").as_bytes());
    push_field(&mut buf, payload_json.as_bytes());
    push_field(&mut buf, event.pre_image_hash.as_deref().unwrap_or("").as_bytes());
    push_field(&mut buf, &event.created_at.to_be_bytes());
    Ok(blake3::hash(&buf).to_hex().to_string())
}

/// Append a length-prefixed field to the canonical hash buffer: an 8-byte
/// big-endian length followed by the raw bytes.
fn push_field(buf: &mut Vec<u8>, value: &[u8]) {
    buf.extend_from_slice(&(value.len() as u64).to_be_bytes());
    buf.extend_from_slice(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The consultative writer's `findings` key is read back verbatim;
    /// pre-#136 aliases keep reading; no usable string ⇒ `None`.
    #[test]
    fn finding_text_prefers_consultative_findings_and_keeps_legacy_aliases() {
        let cited = vec!["ev-1".to_owned(), "ev-2".to_owned()];
        let payload =
            consult_finding_payload("why?", "the lexer drops trailing commas", &cited, Some("h1"));

        assert_eq!(finding_text(&payload), Some("the lexer drops trailing commas"));
        assert_eq!(payload["consultative"], json!(true));
        assert_eq!(payload["question"], json!("why?"));
        assert_eq!(payload["supporting_evidence_ids"], json!(["ev-1", "ev-2"]));
        assert_eq!(payload["hypothesis_id"], json!("h1"));

        // Pre-#136 shapes keep reading: `summary`, then `content`.
        assert_eq!(finding_text(&json!({"summary": "legacy"})), Some("legacy"));
        assert_eq!(finding_text(&json!({"content": "older"})), Some("older"));
        assert_eq!(
            finding_text(&json!({"summary": "a", "content": "b"})),
            Some("a"),
            "first matching key wins, summary before content"
        );

        // No usable string: None — the caller supplies its own fallback and
        // never silently drops the fact.
        assert_eq!(finding_text(&json!({"summary": 7})), None);
        assert_eq!(finding_text(&json!({})), None);
    }

    /// Builder shape → accessor round-trip; missing path/input ⇒ `None`,
    /// never a fabricated path.
    #[test]
    fn write_applied_payload_reads_back_through_write_applied_path() {
        let input = json!({"op": "write", "path": "notes.md", "content": "x"});
        let pre_images = BTreeMap::from([("notes.md".to_owned(), "hash-1".to_owned())]);
        let payload = write_applied_payload("filesystem", &input, &pre_images, None);

        assert_eq!(write_applied_path(&payload), Some("notes.md"));
        assert_eq!(payload["tool"], json!("filesystem"));
        assert_eq!(payload["input"], input);
        assert_eq!(payload["policy_verdict"], json!("allow"));
        assert_eq!(payload["pre_images"], json!({"notes.md": "hash-1"}));
        // ADR-82 slice 1: no registered-name echo for a non-alias request.
        assert_eq!(write_applied_registered_as(&payload), None);

        // ADR-82 slice 1: a canonicalized alias records the canonical `tool`
        // and echoes the registered name it was invoked under.
        let alias = write_applied_payload("filesystem", &input, &pre_images, Some("write"));
        assert_eq!(alias["tool"], json!("filesystem"));
        assert_eq!(write_applied_registered_as(&alias), Some("write"));

        assert_eq!(write_applied_path(&json!({"input": {"op": "read"}})), None);
        assert_eq!(write_applied_path(&json!({"tool": "filesystem"})), None);
        assert_eq!(write_applied_path(&json!({})), None);
    }

    /// `write_applied_paths` is the defensive multi-shape extractor the
    /// rebuild fold uses: `pre_images` keys win, else the scalar path fields.
    #[test]
    fn write_applied_paths_handles_every_payload_shape() {
        assert_eq!(
            write_applied_paths(&json!({ "pre_images": { "a.md": "pre-a", "b.rs": "pre-b" } })),
            vec!["a.md", "b.rs"]
        );
        assert_eq!(write_applied_paths(&json!({ "path": "x.txt" })), vec!["x.txt"]);
        assert_eq!(write_applied_paths(&json!({ "target": "y.txt" })), vec!["y.txt"]);
        assert_eq!(write_applied_paths(&json!({ "input": { "path": "z.txt" } })), vec!["z.txt"]);
        assert!(
            write_applied_paths(&json!({ "something": "else" })).is_empty(),
            "a payload without path info names no files"
        );
    }

    /// Evidence-id accessor is total: absent key, non-array, mixed types.
    #[test]
    fn supporting_evidence_ids_is_total_over_payload_shapes() {
        assert_eq!(
            supporting_evidence_ids(&json!({"supporting_evidence_ids": ["ev-1", "ev-2"]})),
            vec!["ev-1", "ev-2"]
        );
        assert_eq!(
            supporting_evidence_ids(&json!({"supporting_evidence_ids": ["ev-1", 3, "ev-2"]})),
            vec!["ev-1", "ev-2"],
            "non-string entries are skipped, not fatal"
        );
        assert!(
            supporting_evidence_ids(&json!({"supporting_evidence_ids": "not-an-array"})).is_empty(),
            "a non-array value cites nothing"
        );
        assert!(supporting_evidence_ids(&json!({})).is_empty(), "absent key cites nothing");
    }

    /// Views are total: tool/success/args/paths read, failure and empty
    /// payloads degrade instead of panicking.
    #[test]
    fn tool_executed_view_reads_tool_success_args_and_paths() {
        let payload = json!({
            "tool": "shell",
            "args": {"command": "cargo test"},
            "success": true,
            "paths": [{"path": "src/lib.rs"}, {"other": "ignored"}, {"path": "src/main.rs"}],
        });
        let view = tool_executed_view(&payload);
        assert_eq!(view.tool, Some("shell"));
        assert_eq!(view.args, &json!({"command": "cargo test"}));
        assert!(view.success, "success: true is honoured");
        assert_eq!(view.paths, vec!["src/lib.rs", "src/main.rs"], "path rows in order");

        // Failure and degenerate payloads are total views, not panics.
        let failed_payload = json!({"tool": "shell", "success": false});
        let failed = tool_executed_view(&failed_payload);
        assert_eq!(failed.tool, Some("shell"));
        assert!(!failed.success);
        assert!(failed.paths.is_empty());

        let empty_payload = json!({});
        let empty = tool_executed_view(&empty_payload);
        assert_eq!(empty.tool, None);
        assert_eq!(empty.args, &json!(null), "missing args reads as Null");
        assert!(!empty.success);
        assert!(empty.paths.is_empty());
    }

    /// W1 S2: an explicit `outcome` key classifies the execution four ways.
    #[test]
    fn tool_executed_view_reads_explicit_outcome() {
        for (raw, expected) in [
            ("ok", ToolOutcome::Ok),
            ("failed", ToolOutcome::Failed),
            ("denied", ToolOutcome::Denied),
            ("interrupted", ToolOutcome::Interrupted),
        ] {
            let payload = json!({"tool": "shell", "success": false, "outcome": raw});
            let view = tool_executed_view(&payload);
            assert_eq!(
                view.outcome, expected,
                "explicit outcome {raw:?} classifies as {expected:?}"
            );
        }
    }

    /// Without `outcome`, the legacy `success` flag decides; missing/unrecognized
    /// values read `Unknown` — never a guess.
    #[test]
    fn tool_executed_view_falls_back_to_success_without_outcome_key() {
        let ok_payload = json!({"tool": "shell", "success": true});
        let ok = tool_executed_view(&ok_payload);
        assert_eq!(ok.outcome, ToolOutcome::Ok, "legacy success stays Ok");

        let failed_payload = json!({"tool": "shell", "success": false});
        let failed = tool_executed_view(&failed_payload);
        assert_eq!(failed.outcome, ToolOutcome::Failed, "legacy failure stays Failed");

        let missing_payload = json!({"tool": "shell"});
        let missing = tool_executed_view(&missing_payload);
        assert_eq!(missing.outcome, ToolOutcome::Unknown, "missing keys read Unknown");

        let empty_payload = json!({});
        let empty = tool_executed_view(&empty_payload);
        assert_eq!(empty.outcome, ToolOutcome::Unknown, "empty payload reads Unknown");

        let bogus_payload = json!({"tool": "shell", "success": true, "outcome": "x"});
        let bogus = tool_executed_view(&bogus_payload);
        assert_eq!(bogus.outcome, ToolOutcome::Unknown, "unrecognized outcome is Unknown");
    }

    /// `exit_code` is total: numeric in-range values project; anything else
    /// reads `None`.
    #[test]
    fn tool_executed_view_exit_code_projects_only_numeric_in_range_values() {
        let in_range_payload = json!({"tool": "shell", "exit_code": 2});
        let in_range = tool_executed_view(&in_range_payload);
        assert_eq!(in_range.exit_code, Some(2));

        let out_of_range_payload = json!({"tool": "shell", "exit_code": i64::MAX});
        let out_of_range = tool_executed_view(&out_of_range_payload);
        assert_eq!(out_of_range.exit_code, None, "i64 values beyond i32 read None");

        let non_numeric_payload = json!({"tool": "shell", "exit_code": "0"});
        let non_numeric = tool_executed_view(&non_numeric_payload);
        assert_eq!(non_numeric.exit_code, None);

        let null_payload = json!({"tool": "shell", "exit_code": null});
        let null = tool_executed_view(&null_payload);
        assert_eq!(null.exit_code, None);
    }

    /// The content hash is deterministic over caller-attested fields and
    /// excludes log-assigned sequencing (`gate_seq`/`agent_seq` never enter:
    /// the function takes only the `NewWhiteboardEvent` attestation).
    #[test]
    fn content_hash_is_deterministic_and_excludes_sequencing() {
        let base = NewWhiteboardEvent {
            event_id: "same-content".to_owned(),
            agent_id: "agent-a".to_owned(),
            kind: crate::whiteboard::WhiteboardKind::Decision,
            scope: String::new(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload: json!({ "note": "event" }),
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        };
        let twin = base.clone();
        let hash_a = compute_content_hash(&base).expect("hash a");
        let hash_b = compute_content_hash(&twin).expect("hash b");
        assert_eq!(hash_a, hash_b, "identical caller attestation => identical hash");

        let mut changed = base.clone();
        changed.payload = json!({ "note": "different" });
        assert_ne!(
            compute_content_hash(&changed).expect("hash changed"),
            hash_a,
            "content is part of the hash"
        );

        // Optional fields contribute empty strings, so `None` and `Some("")`
        // attestation hash equal — sequencing is simply not in the input.
        let mut with_empty_optionals = base.clone();
        with_empty_optionals.session_id = Some(String::new());
        with_empty_optionals.plan_id = Some(String::new());
        with_empty_optionals.causation = Some(String::new());
        with_empty_optionals.pre_image_hash = Some(String::new());
        assert_eq!(
            compute_content_hash(&with_empty_optionals).expect("hash empty optionals"),
            hash_a,
            "empty optional attestation fields hash the same as None"
        );
    }
}
