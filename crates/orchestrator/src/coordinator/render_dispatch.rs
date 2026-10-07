//! Render the Coordinator's dispatch-decision prompt sections (NORM S9).
//!
//! This module owns the pure string assembly for the dispatch prompt: the
//! specialist roster, the settled-dispatch history, the stage-phase marker,
//! the cycle-ceiling and suitability advisories, and the full system prompt
//! that stacks them (extracted verbatim from `coordinator.rs`).
//!
//! Every section is advisory context for the Coordinator's model: it states
//! what is known and never gates, selects, or orders a dispatch. The bodies
//! take all context explicitly (no store, bus, or policy I/O) and emit
//! byte-stable text for identical inputs, so the prompt snapshots in the
//! coordinator test suite hold unchanged.

use concerto_config::StageKind;
use concerto_core::types::{AgentId, AgentRunResult, AgentStage, AgentTask, TaskId};

use super::{
    binding_doc, bounded_text, derive_run_phase, execution_stage_tag, is_code_artifact_path,
    kind_stage_tag, outcome_label, CoordinatorAgent, DispatchLedger, DispatchSessionState,
    COORDINATOR_DISPATCH_PROMPT,
};

/// One settled-dispatch observation for the dispatch-history section: the
/// role, the short outcome word, and the artifacts it produced. Borrowed from
/// the in-memory ledger so rendering performs no store read.
pub(super) struct DispatchObservation<'a> {
    pub(super) role: &'a AgentId,
    pub(super) outcome: &'static str,
    pub(super) files: &'a [camino::Utf8PathBuf],
}

/// Render the objective's settled dispatch history as bounded advisory
/// context: per role (iterated from the registry ids, so no role name is ever
/// part of the logic), the settled dispatch count, the last outcome word, and
/// the code-vs-docs artifact split (`is_code_artifact_path` over the ledger
/// files). Only counts and short words are emitted — never a path, never a
/// directive. A fresh run renders the nothing-yet line. `observations` must be
/// in dispatch order so "last" is meaningful.
pub(super) fn render_dispatch_history_section(
    registry_ids: &[AgentId],
    observations: &[DispatchObservation<'_>],
) -> String {
    let mut out = String::from(
        "\n[Dispatch history (advisory — settled dispatches on this objective only; counts, \
         last outcome word, and the code/doc artifact split; never a rule and never an \
         instruction to call anyone)]\n",
    );
    let mut ids: Vec<&AgentId> = registry_ids.iter().collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut any = false;
    for id in ids {
        let role_observations: Vec<&DispatchObservation<'_>> =
            observations.iter().filter(|obs| obs.role == id).collect();
        if role_observations.is_empty() {
            continue;
        }
        any = true;
        let count = role_observations.len();
        let last = role_observations.last().map(|obs| obs.outcome).unwrap_or("unknown");
        let mut code = 0usize;
        let mut docs = 0usize;
        for obs in &role_observations {
            for path in obs.files {
                if is_code_artifact_path(path) {
                    code += 1;
                } else {
                    docs += 1;
                }
            }
        }
        out.push_str(&format!(
            "- {id}: {count} dispatch(es), last {last}, {code} code / {docs} doc artifact(s)\n"
        ));
    }
    if !any {
        out.push_str("- none yet\n");
    }
    out
}

/// Render a config `AgentCapabilities` as a compact readable list for the
/// roster (declared metadata, not enforcement).
fn render_agent_capabilities(caps: &concerto_config::AgentCapabilities) -> String {
    let mut parts: Vec<String> = Vec::new();
    if caps.fs_read.unwrap_or(false) {
        parts.push("fs_read".to_owned());
    }
    if caps.fs_write.unwrap_or(false) {
        parts.push("fs_write".to_owned());
    }
    if caps.shell.unwrap_or(false) {
        parts.push("shell".to_owned());
    }
    if caps.git.unwrap_or(false) {
        parts.push("git".to_owned());
    }
    if caps.lsp.unwrap_or(false) {
        parts.push("lsp".to_owned());
    }
    if caps.eval.unwrap_or(false) {
        parts.push("eval".to_owned());
    }
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join(", ")
    }
}

/// Render an `OutputMode` in its serialized snake_case form for the roster.
fn render_output_mode(mode: concerto_core::types::OutputMode) -> &'static str {
    match mode {
        concerto_core::types::OutputMode::Freeform => "freeform",
        concerto_core::types::OutputMode::DesignDoc => "design_doc",
        concerto_core::types::OutputMode::ResearchReport => "research_report",
        concerto_core::types::OutputMode::ReviewReport => "review_report",
    }
}

impl CoordinatorAgent {
    /// The roster the Coordinator decides from (ADR-35 amendment 2026-09-05
    /// §1): every REGISTERED agent's id, name, role, stage (informational
    /// metadata), declared capabilities, output mode, and a bounded
    /// system-instruction excerpt. Deleted/disabled agents are absent by
    /// construction (the registry never holds them) — this is how the
    /// Coordinator "knows" who to call: context, not policy.
    fn render_specialist_roster(&self, task: &AgentTask, ledger: &DispatchLedger) -> String {
        let mut ids = self.registry.ids();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut out = String::from("[Available specialists]\n");
        for id in ids {
            let Some(agent) = self.registry.get(&id) else { continue };
            let config = self.registry.config(&id);
            let name = config.map(|config| config.name.clone()).unwrap_or_else(|| id.to_string());
            let role = config.map(|config| config.role.clone()).unwrap_or_default();
            let stage = agent
                .stage()
                .map(|stage| stage.as_str().to_owned())
                .unwrap_or_else(|| "freeform".into());
            let capabilities = config
                .map(|config| render_agent_capabilities(&config.capabilities))
                .unwrap_or_else(|| "none".to_owned());
            let output_mode =
                config.map(|config| render_output_mode(config.output_mode)).unwrap_or("freeform");
            let instructions = config
                .map(|config| bounded_text(&config.prompt_sections.system_instructions, 400))
                .unwrap_or_default();
            out.push_str(&format!(
                "- id: {id}\n  name: {name}\n  role: {role}\n  stage: {stage} \
                 (informational — never a dispatch rule)\n  capabilities: {capabilities}\n  \
                 output_mode: {output_mode}\n  instructions: {instructions}\n"
            ));
        }
        out.push_str(&self.render_suitability_advisory(task));
        out.push_str(&self.render_dispatch_history(ledger));
        out
    }

    /// The objective's settled dispatch observations in dispatch order — the
    /// in-memory ledger's completed results, ordered by their (Ulid,
    /// time-ordered) subtask id. No store read happens here: the ledger is the
    /// same accumulator that checkpoints and restores, so a resumed run renders
    /// the same history without a DB round-trip.
    fn dispatch_observations<'a>(
        &self,
        ledger: &'a DispatchLedger,
    ) -> Vec<DispatchObservation<'a>> {
        let mut entries: Vec<(&TaskId, &AgentRunResult)> =
            ledger.completed_results.iter().collect();
        entries.sort_by_key(|(task_id, _)| task_id.0);
        entries
            .into_iter()
            .map(|(_, result)| DispatchObservation {
                role: &result.role,
                outcome: outcome_label(&result.outcome),
                files: &result.files_modified,
            })
            .collect()
    }

    /// Issue: the agent-agnostic dispatch-history section rendered under the
    /// roster. Agent-agnostic by construction: the free renderer walks the
    /// REGISTRY ids and attributes each role's settled outcomes from the
    /// ledger — no role name appears in the logic and no directive language is
    /// emitted. Advisory only: it states what happened, never what to call.
    pub(super) fn render_dispatch_history(&self, ledger: &DispatchLedger) -> String {
        let ids = self.registry.ids();
        let observations = self.dispatch_observations(ledger);
        render_dispatch_history_section(&ids, &observations)
    }

    /// The one-line stage-phase marker rendered ABOVE the roster. The phase is
    /// derived deterministically from present artifacts: a design document's
    /// presence/binding (`binding_doc` — verified or approved), whether any
    /// code artifact exists in the ledger, and whether a review-stage dispatch
    /// has settled (`role_in_kind_stage` — stage kinds from config, never role
    /// names). Advisory context only; it never gates or selects a dispatch.
    pub(super) fn render_phase_marker(
        &self,
        state: &DispatchSessionState,
        ledger: &DispatchLedger,
    ) -> String {
        let has_doc = state.doc.is_some();
        let doc_binds = binding_doc(state).is_some();
        let code_present = ledger.all_files.iter().any(|path| is_code_artifact_path(path));
        let reviewed = ledger.completed_results.values().any(|result| {
            self.role_in_kind_stage(&result.role, StageKind::Review, AgentStage::is_review)
        });
        let phase = derive_run_phase(has_doc, doc_binds, code_present, reviewed);
        format!("[Run phase (advisory): {} — {}]\n\n", phase.as_str(), phase.meaning())
    }

    /// ADR-35 amendment (2026-09-16 §2): the configured review/validation
    /// cycle ceilings are ADVISORY context injected into the dispatch prompt —
    /// a "typical ceiling" the Coordinator may weigh, never a hardcoded loop
    /// bound or terminal stop. Gate roles are resolved by stage KIND (never by
    /// role id) and the ceiling comes from the collaboration rule, falling
    /// back to the stage's configured/kind default. Absent gate roles render
    /// nothing.
    fn render_cycle_ceiling_advisory(&self) -> String {
        let implements = AgentId::new(execution_stage_tag(self.blueprint_facade.as_ref()));
        let ceiling = |kind: StageKind, fallback_tag: &'static str| -> Option<u32> {
            let tag = kind_stage_tag(self.blueprint_facade.as_ref(), kind, fallback_tag);
            let from = self.first_agent_for_stage(&AgentStage::new(&tag))?;
            let kind_default = self
                .blueprint_facade
                .as_ref()
                .and_then(|facade| facade.stage_by_tag(&tag))
                .and_then(|stage| stage.def.max_cycles)
                .unwrap_or_else(|| kind.default_max_cycles());
            Some(self.relationships.max_cycles(&from, &implements, kind_default))
        };
        let mut parts = Vec::new();
        if let Some(n) = ceiling(StageKind::Review, AgentStage::REVIEW) {
            parts.push(format!("review {n}"));
        }
        if let Some(n) = ceiling(StageKind::Acceptance, AgentStage::VALIDATE) {
            parts.push(format!("validation {n}"));
        }
        if parts.is_empty() {
            return String::new();
        }
        format!(
            "[Cycle ceilings (advisory): {} — typical limits you may weigh; they never \
             terminate the run or force a dispatch.]\n\n",
            parts.join(", ")
        )
    }

    /// Issue #60: the suitability ranking as ADVISORY context under the
    /// roster — the deterministic, decayed dispatch-outcome evidence for
    /// the run's task class, ranked with bounded reasons (no cost/spend/
    /// latency/model-quality inputs exist on the scoring path). ADVICE,
    /// NOT a gate: the Coordinator's model still decides who to call, and
    /// a candidate absent from the section (fresh history) is not
    /// disadvantaged — the ranking ordering only ranks; no dispatch rule
    /// reacts to it. Rendered ONCE per decision session, exactly where
    /// the candidate roles live.
    fn render_suitability_advisory(&self, task: &AgentTask) -> String {
        let ids = self.registry.ids();
        if ids.is_empty() || self.suitability.is_empty() {
            return String::new();
        }
        let class = crate::suitability::TaskClass::derive(&task.description, &[]);
        let candidate_ids: Vec<String> = ids.iter().map(|id| id.as_str().to_owned()).collect();
        let now = time::OffsetDateTime::now_utc();
        let ranking = self.suitability.rank(&candidate_ids, class, now);
        let mut out = String::from(
            "\n[Suitability signal (advisory — measured from past dispatch outcomes for              this task class; not a rule, never a dispatch requirement; contains no cost,              spend, or workload inputs)]\n",
        );
        for entry in ranking {
            let score = if entry.score_milli < 0 {
                format!("-{}", entry.score_milli.abs())
            } else {
                format!("+{}", entry.score_milli)
            };
            let reasons = crate::suitability::bound_reasons(&entry.reasons);
            out.push_str(&format!("- {}: score {} — {reasons}\n", entry.agent_id, score));
        }
        out
    }

    /// The full system prompt for the Coordinator's decision loop: built-in
    /// instructions, the roster, the run's evidence/provenance intro, the
    /// session skills section, and the Studio's supplemental prompt.
    pub(super) fn render_dispatch_system_prompt(
        &self,
        task: &AgentTask,
        intro: &str,
        dispatching: bool,
        state: &DispatchSessionState,
        ledger: &DispatchLedger,
    ) -> String {
        let mut prompt = String::new();
        if dispatching {
            prompt.push_str(COORDINATOR_DISPATCH_PROMPT);
            prompt.push_str("\n\n");
            // The stage-phase marker sits directly above the roster — one
            // advisory line derived from present artifacts.
            prompt.push_str(&self.render_phase_marker(state, ledger));
            prompt.push_str(&self.render_cycle_ceiling_advisory());
            prompt.push_str(&self.render_specialist_roster(task, ledger));
        } else {
            prompt.push_str(
                "You are the Coordinator. This run is PLANNING-ONLY: produce the plan for \
                 the objective below as structured prose. Do NOT attempt to dispatch or \
                 execute anything — no tools are available to you.\n\n",
            );
        }
        if !intro.is_empty() {
            prompt.push_str(intro);
            prompt.push_str("\n\n");
        }
        if !self.skills_section.is_empty() {
            prompt.push_str(&self.skills_section);
            prompt.push_str("\n\n");
        }
        // Project AGENTS.md context (ADR-70): injected between the skills
        // section and the environment card. Run-scoped and refreshed once at
        // run start by the runtime; `section()` is a cheap clone, so no
        // filesystem work happens in the prompt hot path.
        if let Some(project_context) = &self.project_context {
            let section = project_context.section();
            if !section.is_empty() {
                prompt.push_str(&section);
                prompt.push_str("\n\n");
            }
        }
        // ── Dispatch budget advisory (ADR-35 amendment 2026-09-16 §6) ────
        // The run-wide dispatch ceiling (ADR-52 `max_total_iterations`) is
        // the Coordinator's hard budget. Publishing it up front lets the
        // model decide early whether to invoke `request_user_input` for
        // operator guidance instead of spending its remaining specialist
        // calls on low-confidence work. Emitted only while dispatching and
        // only when a ceiling is configured.
        if dispatching {
            if let Some(cap) = self.max_total_iterations {
                let used = self.model_dispatch_count.min(cap);
                let remaining = cap - used;
                prompt.push_str(&format!(
                    "<dispatch_budget>\nRun-wide ceiling: {cap} specialist calls; {used} used, \
                     {remaining} remaining. Spend them deliberately: a specialist call is the \
                     default way work advances, so do not hoard the budget by working in-house. \
                     The ceiling exists to force prioritization, not to price a needed dispatch \
                     against doing the work yourself. Prefer request_user_input only when the \
                     path ahead is genuinely low-confidence.\n</dispatch_budget>\n\n"
                ));
            }
        }
        prompt.push_str(&format!("Objective: {}\n", task.description));
        // Issue #56: the Coordinator's decisions consume the structured
        // world model — a bounded rendered block rides every dispatch-
        // decision prompt (the projection is refreshed by the session
        // entry; the block is character-bounded by the builder's render).
        if !self.world_model.is_empty_beyond_objective() {
            prompt.push_str(&self.world_model.render());
            prompt.push('\n');
        }
        if !self.supplemental_prompt.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&self.supplemental_prompt);
        }
        // The OS/shell identity card and the working-memory block are added by
        // the shared `PromptBuilder` seam in `build_dispatch_system_message`
        // below. The template ends with the same `{working_memory}` separator
        // the single-agent templates use, so an empty block removes it cleanly
        // and a non-empty block is substituted in place (default) or appended
        // as a volatile tail under `cache_stable_prefix`.
        prompt.push_str(crate::prompts::WORKING_MEMORY_SEPARATOR_PLACEHOLDER);
        prompt
    }
}

#[cfg(test)]
mod tests {
    use concerto_config::AgentCapabilities;
    use concerto_core::types::AgentId;

    use super::{
        render_agent_capabilities, render_dispatch_history_section, render_output_mode,
        DispatchObservation,
    };

    /// The advisory header byte-for-byte as the renderer emits it — the
    /// coordinator's prompt snapshots depend on every byte, so the golden
    /// repeats the renderer's own continued literal.
    const HISTORY_HEADER: &str = "\n[Dispatch history (advisory — settled dispatches on this objective only; counts, \
                                  last outcome word, and the code/doc artifact split; never a rule and never an \
                                  instruction to call anyone)]\n";

    /// Render a fresh objective's history byte-exactly: the advisory header
    /// plus the nothing-yet line — whether or not roles are registered, and
    /// with an empty registry too (the empty-input edge).
    #[test]
    fn render_dispatch_history_section_reports_none_yet_byte_exactly_for_a_fresh_run() {
        let empty_registry = render_dispatch_history_section(&[], &[]);
        assert_eq!(
            empty_registry,
            format!("{HISTORY_HEADER}- none yet\n"),
            "an empty registry renders header + nothing-yet"
        );

        let ids = vec![AgentId::new("architect"), AgentId::new("coder")];
        let registered = render_dispatch_history_section(&ids, &[]);
        assert_eq!(
            registered,
            format!("{HISTORY_HEADER}- none yet\n"),
            "registered roles without settled dispatches render no per-role lines"
        );
    }

    /// Render one role's settled dispatches byte-exactly: count, last outcome
    /// word, and the code/doc artifact split — never a path, never a directive.
    #[test]
    fn render_dispatch_history_section_renders_settled_counts_outcomes_and_split_byte_exactly() {
        let coder = AgentId::new("coder");
        let ids = vec![coder.clone()];
        let files = vec![
            camino::Utf8PathBuf::from("src/lib.rs"),
            camino::Utf8PathBuf::from("docs/plan.md"),
        ];
        let observations =
            vec![DispatchObservation { role: &coder, outcome: "success", files: &files }];

        let out = render_dispatch_history_section(&ids, &observations);

        assert_eq!(
            out,
            format!(
                "{HISTORY_HEADER}- coder: 1 dispatch(es), last success, 1 code / 1 doc artifact(s)\n"
            ),
            "count, last outcome, and artifact split render byte-exactly"
        );
    }

    /// Render mixed roles byte-exactly in sorted registry order — the
    /// renderer sorts by id, never by dispatch order, and attributes each
    /// role only its own outcomes.
    #[test]
    fn render_dispatch_history_section_renders_roles_in_sorted_registry_order_byte_exactly() {
        let architect = AgentId::new("architect");
        let coder = AgentId::new("coder");
        let reviewer = AgentId::new("reviewer");
        // Registry order is deliberately unsorted: the renderer owns the order.
        let ids = vec![reviewer.clone(), coder.clone(), architect.clone()];
        let files = vec![camino::Utf8PathBuf::from("src/x.rs")];
        let observations = vec![
            DispatchObservation { role: &reviewer, outcome: "blocked", files: &[] },
            DispatchObservation { role: &coder, outcome: "success", files: &files },
            DispatchObservation { role: &architect, outcome: "success", files: &[] },
        ];

        let out = render_dispatch_history_section(&ids, &observations);

        assert_eq!(
            out,
            format!(
                "{HISTORY_HEADER}- architect: 1 dispatch(es), last success, 0 code / 0 doc \
                 artifact(s)\n\
                 - coder: 1 dispatch(es), last success, 1 code / 0 doc artifact(s)\n\
                 - reviewer: 1 dispatch(es), last blocked, 0 code / 0 doc artifact(s)\n"
            ),
            "roles render in sorted id order, each attributed exactly"
        );
    }

    /// A long dispatch history (200 settled observations) stays bounded to
    /// exactly one line per registered role with the correct aggregate counts.
    #[test]
    fn render_dispatch_history_section_bounds_a_long_dispatch_history() {
        let architect = AgentId::new("architect");
        let coder = AgentId::new("coder");
        let ids = vec![architect.clone(), coder.clone()];
        let code_files = vec![camino::Utf8PathBuf::from("src/lib.rs")];
        let doc_files = vec![camino::Utf8PathBuf::from("docs/plan.md")];
        let mut observations = Vec::new();
        for _ in 0..100 {
            observations.push(DispatchObservation {
                role: &architect,
                outcome: "success",
                files: &doc_files,
            });
        }
        for _ in 0..100 {
            observations.push(DispatchObservation {
                role: &coder,
                outcome: "success",
                files: &code_files,
            });
        }

        let out = render_dispatch_history_section(&ids, &observations);

        assert_eq!(
            out,
            format!(
                "{HISTORY_HEADER}- architect: 100 dispatch(es), last success, 0 code / 100 doc \
                 artifact(s)\n\
                 - coder: 100 dispatch(es), last success, 100 code / 0 doc artifact(s)\n"
            ),
            "a long history stays bounded to one line per registered role"
        );
    }

    /// Special-character artifact paths (spaces, parentheses, an em dash, a
    /// non-ASCII stem) are classified by extension only and never emitted.
    #[test]
    fn render_dispatch_history_section_counts_special_character_paths_without_emitting_them() {
        let coder = AgentId::new("coder");
        let ids = vec![coder.clone()];
        let files = vec![
            camino::Utf8PathBuf::from("src/über.rs"),
            camino::Utf8PathBuf::from("docs/plan (final) — v2.md"),
        ];
        let observations =
            vec![DispatchObservation { role: &coder, outcome: "failed", files: &files }];

        let out = render_dispatch_history_section(&ids, &observations);

        assert_eq!(
            out,
            format!("{HISTORY_HEADER}- coder: 1 dispatch(es), last failed, 1 code / 1 doc artifact(s)\n"),
            "special-character paths are counted by extension only"
        );
        assert!(
            !out.contains("über") && !out.contains("plan (final)"),
            "artifact paths never render: {out}"
        );
    }

    /// Capabilities with every flag unset — or explicitly false — render the
    /// `none` word (the empty-input edge for the roster line).
    #[test]
    fn render_agent_capabilities_renders_none_when_every_flag_is_unset() {
        let unset = AgentCapabilities {
            fs_read: None,
            fs_write: None,
            shell: None,
            git: None,
            lsp: None,
            eval: None,
        };
        assert_eq!(render_agent_capabilities(&unset), "none");

        let disabled = AgentCapabilities {
            fs_read: Some(false),
            fs_write: Some(false),
            shell: Some(false),
            git: Some(false),
            lsp: Some(false),
            eval: Some(false),
        };
        assert_eq!(render_agent_capabilities(&disabled), "none");
    }

    /// Declared flags render in their fixed declaration order as a compact
    /// comma-separated list; a single flag renders without separators.
    #[test]
    fn render_agent_capabilities_renders_declared_flags_in_declaration_order() {
        let all = AgentCapabilities {
            fs_read: Some(true),
            fs_write: Some(true),
            shell: Some(true),
            git: Some(true),
            lsp: Some(true),
            eval: Some(true),
        };
        assert_eq!(render_agent_capabilities(&all), "fs_read, fs_write, shell, git, lsp, eval");

        let readonly = AgentCapabilities {
            fs_read: Some(true),
            fs_write: None,
            shell: None,
            git: None,
            lsp: None,
            eval: None,
        };
        assert_eq!(render_agent_capabilities(&readonly), "fs_read");
    }

    /// Every `OutputMode` renders its serialized snake_case label — the roster
    /// line must match the config wire form exactly.
    #[test]
    fn render_output_mode_renders_serialized_snake_case_labels() {
        use concerto_core::types::OutputMode;
        assert_eq!(render_output_mode(OutputMode::Freeform), "freeform");
        assert_eq!(render_output_mode(OutputMode::DesignDoc), "design_doc");
        assert_eq!(render_output_mode(OutputMode::ResearchReport), "research_report");
        assert_eq!(render_output_mode(OutputMode::ReviewReport), "review_report");
    }
}
