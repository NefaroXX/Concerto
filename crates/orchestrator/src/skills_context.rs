//! Runtime-owned skills context (ADR-43, Task 4).
//!
//! [`SkillsContext`] is the orchestrator-side handle that discovers enabled
//! skill packs, formats their instructions into one bounded markdown section,
//! and serves that section to every prompt path (the single-agent
//! `PromptBuilder` and the coordinator specialist assembly). Reading the
//! section is cheap: it is formatted once in [`SkillsContext::refresh`] and
//! cloned out per call, so no filesystem or parsing work happens in the
//! prompt hot path.
//!
//! The context is **fail-soft** by design: discovery is warn-and-continue —
//! a broken skill pack is logged and skipped, never aborting the scan — so
//! an unreadable pack can never crash the agent loop. (This mirrors the
//! `concerto-skills` manager contract, where discovery itself warns and skips
//! malformed packs; this layer therefore normally sees discovery succeed.)
//! `refresh` still surfaces a *genuinely fatal* scan error (returning `Err`
//! and keeping the previous section) so the caller can decide how to surface
//! it, but a single broken pack is not one of those.

use std::sync::Arc;
use std::sync::RwLock;

use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_skills::{SkillDescriptor, SkillManager, SkillsError};
use tracing::{error, info, warn};

/// Default budget for the injected skills section (characters) when
/// `SkillsConfig.max_chars` is unset. Mirrors the budget documented in
/// `docs/config.toml.example`.
pub const DEFAULT_SKILLS_BUDGET_CHARS: usize = 4000;

/// Marker appended when the assembled skills section exceeds the budget.
const TRUNCATION_MARKER: &str = "\n\n[truncated: skill instructions exceed the session budget]";

/// Header + lead-in emitted only when at least one skill is enabled.
const SECTION_HEADER: &str = "## Skills\nThe following skill instructions apply to this session and MUST be followed when relevant:";

/// Mutable state behind the context's read/write lock.
#[derive(Debug, Default)]
struct SkillsState {
    /// Config-level enablement. `Some(ids)` restricts to those ids; `None`
    /// defers to `auto_load`.
    enabled_ids: Option<Vec<String>>,
    /// Resolved descriptors from the last successful refresh.
    descriptors: Vec<SkillDescriptor>,
    /// The formatted, budgeted section served to prompts.
    section: String,
}

/// Runtime-owned handle to the enabled skill packs for a session (ADR-43
/// decision 5).
///
/// Constructed once from configuration and shared (as `Arc<SkillsContext>`)
/// by both prompt paths so a live toggle (Task 7) takes effect on the next
/// prompt build after a refresh. The constructor performs no filesystem work;
/// call [`SkillsContext::refresh`] once at startup.
#[derive(Debug)]
pub struct SkillsContext {
    manager: Arc<SkillManager>,
    auto_load: bool,
    max_chars: usize,
    state: RwLock<SkillsState>,
}

impl Default for SkillsContext {
    fn default() -> Self {
        Self::disabled()
    }
}

impl SkillsContext {
    /// Create a context with empty state. No filesystem access happens here.
    ///
    /// * `manager` — discovery handle over the configured search paths.
    /// * `enabled_ids` — `Some(ids)` restricts to those skill ids (an empty
    ///   list disables everything); `None` defers to `auto_load`.
    /// * `auto_load` — when `enabled_ids` is `None`, load every discovered
    ///   skill.
    /// * `max_chars` — hard character budget for the injected section.
    pub fn new(
        manager: Arc<SkillManager>,
        enabled_ids: Option<Vec<String>>,
        auto_load: bool,
        max_chars: usize,
    ) -> Self {
        Self {
            manager,
            auto_load,
            max_chars,
            state: RwLock::new(SkillsState {
                enabled_ids,
                descriptors: Vec::new(),
                section: String::new(),
            }),
        }
    }

    /// Build a context from `SkillsConfig` (ADR-43). `None` — or a config
    /// with `enabled = false` — yields a fully disabled context: discovery is
    /// a no-op and the section is always empty. `max_chars` falls back to
    /// [`DEFAULT_SKILLS_BUDGET_CHARS`] when unset.
    ///
    /// No filesystem work happens here; call [`SkillsContext::refresh`] once
    /// at startup.
    pub fn from_config(config: Option<&concerto_config::SkillsConfig>) -> Self {
        let Some(config) = config.filter(|config| config.enabled) else {
            return Self::disabled();
        };
        let manager = Arc::new(SkillManager::new(
            config.search_paths.iter().map(std::path::PathBuf::from).collect(),
        ));
        Self::new(
            manager,
            config.enabled_ids.clone(),
            config.auto_load,
            config.max_chars.unwrap_or(DEFAULT_SKILLS_BUDGET_CHARS),
        )
    }

    /// A disabled context by construction: empty section and no discovery.
    pub fn disabled() -> Self {
        Self::new(
            Arc::new(SkillManager::new(Vec::new())),
            Some(Vec::new()),
            false,
            DEFAULT_SKILLS_BUDGET_CHARS,
        )
    }

    /// Re-run discovery and enablement resolution, then re-format the section.
    ///
    /// Effective ids = `enabled_ids` when `Some`; otherwise all discovered
    /// skills when `auto_load`; otherwise none. On a discovery error the
    /// previous state (descriptors + section) is kept, the error is logged at
    /// error level, and `Err` is returned so the caller may surface it — the
    /// agent loop must not fail on it.
    ///
    /// When nothing can be enabled (explicit empty allow-list and
    /// `auto_load` off) discovery is skipped entirely, so a disabled
    /// configuration performs no filesystem work.
    pub fn refresh(&self) -> Result<(), SkillsError> {
        let enabled_ids =
            self.state.read().unwrap_or_else(|poison| poison.into_inner()).enabled_ids.clone();
        // Fast path: an explicit empty allow-list with auto_load off can never
        // enable anything, so avoid the discovery walk entirely.
        let needs_discovery =
            enabled_ids.as_deref().is_some_and(|ids| !ids.is_empty()) || self.auto_load;

        let (descriptors, section) = if needs_discovery {
            let discovered = match self.manager.discover() {
                Ok(discovered) => discovered,
                Err(scan_error) => {
                    error!(%scan_error, "skill discovery failed; keeping previous skills state");
                    return Err(scan_error);
                }
            };
            let effective: Vec<SkillDescriptor> = match &enabled_ids {
                Some(_) => self
                    .manager
                    .resolve_enabled(&discovered, enabled_ids.as_deref())
                    .into_iter()
                    .cloned()
                    .collect(),
                None if self.auto_load => discovered,
                None => Vec::new(),
            };
            (effective.clone(), format_section(&effective, self.max_chars))
        } else {
            (Vec::new(), String::new())
        };

        let mut state = self.state.write().unwrap_or_else(|poison| poison.into_inner());
        state.enabled_ids = enabled_ids;
        state.descriptors = descriptors;
        state.section = section;
        Ok(())
    }

    /// The formatted skills section for the current session. Empty when no
    /// skills are enabled. Cheap (one `String` clone); no I/O per prompt build.
    pub fn section(&self) -> String {
        self.state.read().unwrap_or_else(|poison| poison.into_inner()).section.clone()
    }

    /// The resolved skill descriptors from the last successful refresh.
    pub fn descriptors(&self) -> Vec<SkillDescriptor> {
        self.state.read().unwrap_or_else(|poison| poison.into_inner()).descriptors.clone()
    }

    /// Emit the one-per-run skills-injection audit record and an `info` log
    /// line.
    ///
    /// Publishes a single [`EventKind::SkillsInjected`] bound to `session_id`
    /// (persisted by the session event recorder and surfaced as a transcript
    /// activity entry) and logs the same summary at `info` level, so a default
    /// log level proves *which* enabled skill packs — and *how many*
    /// characters against the configured budget — were injected into this
    /// run's system prompt.
    ///
    /// Deliberately content-free: only sorted, deduplicated pack ids and
    /// character counts are reported, never pack instruction text (ADR-43
    /// secrets-adjacent caution). No-op when no section is currently injected,
    /// so a disabled configuration stays silent. Publishing is fail-soft: a
    /// publish error is warned about and the run proceeds.
    pub fn report_injection(&self, bus: &EventBus, session_id: Ulid) {
        let (skill_ids, total_chars, budget_chars) = {
            let state = self.state.read().unwrap_or_else(|poison| poison.into_inner());
            if state.section.is_empty() {
                return;
            }
            let mut ids: Vec<String> = state.descriptors.iter().map(|d| d.id.clone()).collect();
            ids.sort();
            ids.dedup();
            (ids, state.section.chars().count(), self.max_chars)
        };

        info!(
            ?skill_ids,
            total_chars, budget_chars, "skill instructions injected into the session prompt"
        );
        if let Err(error) = bus.publish_for_session(
            session_id,
            Ulid::new(),
            EventKind::SkillsInjected { skill_ids, total_chars, budget_chars },
        ) {
            warn!(%error, %session_id, "failed to publish the skills injection event");
        }
    }

    /// Update config-level enablement live (Task 7 calls this from the UI).
    /// Does **not** re-discover; the caller must call [`SkillsContext::refresh`]
    /// afterwards for the new ids to take effect.
    pub fn set_enabled_ids(&self, enabled_ids: Option<Vec<String>>) {
        self.state.write().unwrap_or_else(|poison| poison.into_inner()).enabled_ids = enabled_ids;
    }
}

/// Assemble the injected section from the resolved descriptors, honoring
/// `max_chars`.
///
/// Algorithm (documented contract):
/// 1. Emit nothing when no skills are enabled (so a disabled configuration
///    adds no `## Skills` header to prompts).
/// 2. Reserve the truncation marker up front: the section content budget is
///    `max_chars - marker_len`. If the marker itself does not fit, output
///    just the marker (char-boundary truncated to `max_chars`).
/// 3. Greedy in id order (deterministic regardless of discovery/enable-list
///    ordering): keep whole skill blocks while they fit; the first block that
///    overflows is truncated into the remaining budget so the last skill is
///    still represented; stop there.
/// 4. Append the truncation marker when any content was cut. The final output
///    is always `<= max_chars` characters.
fn format_section(descriptors: &[SkillDescriptor], max_chars: usize) -> String {
    if descriptors.is_empty() || max_chars == 0 {
        return String::new();
    }
    let marker_chars = TRUNCATION_MARKER.chars().count();
    if max_chars < marker_chars {
        // The marker itself cannot fit; emit only the marker (truncated).
        return truncate_chars(TRUNCATION_MARKER, max_chars);
    }
    let content_budget = max_chars - marker_chars;

    // Deterministic id order: `resolve_enabled` with an explicit allow-list
    // preserves caller order, so re-sort by id here.
    let mut ordered: Vec<&SkillDescriptor> = descriptors.iter().collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));

    let header_chars = SECTION_HEADER.chars().count();
    if header_chars > content_budget {
        // The header alone does not fit; emit only the marker.
        return TRUNCATION_MARKER.to_string();
    }

    let mut out = String::new();
    out.push_str(SECTION_HEADER);
    let mut truncated = false;
    for descriptor in ordered {
        let block = format!("\n{}", format_block(descriptor));
        let block_chars = block.chars().count();
        let used = out.chars().count();
        if used.saturating_add(block_chars) <= content_budget {
            out.push_str(&block);
        } else {
            // Whole block does not fit: keep as much of it as the remaining
            // budget allows so the last skill is still represented.
            let remaining = content_budget.saturating_sub(used);
            if remaining > 0 {
                out.push_str(&truncate_chars(&block, remaining));
            }
            truncated = true;
            break;
        }
    }
    if truncated {
        out.push_str(TRUNCATION_MARKER);
    }
    out
}

/// One skill's block in the section.
fn format_block(descriptor: &SkillDescriptor) -> String {
    format!(
        "### {} ({}, v{})\n{}",
        descriptor.manifest.name,
        descriptor.id,
        descriptor.manifest.version,
        descriptor.instructions,
    )
}

/// First `max_chars` characters of `text` (character-boundary safe).
fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write as _;

    /// Write a minimal `skill.toml` pack with inline instructions.
    fn write_toml_pack(root: &std::path::Path, id: &str, instructions: &str) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).expect("create pack dir");
        let toml = format!(
            "id = \"{id}\"\nname = \"Skill {id}\"\nversion = \"1.0.0\"\ndescription = \"test\"\ninstructions = \"{instructions}\"\n"
        );
        let mut file = fs::File::create(dir.join("skill.toml")).expect("create manifest");
        file.write_all(toml.as_bytes()).expect("write manifest");
    }

    fn manager(root: &std::path::Path) -> Arc<SkillManager> {
        Arc::new(SkillManager::new(vec![root.to_path_buf()]))
    }

    fn refresh_ids(context: &SkillsContext) -> Vec<String> {
        context.refresh().expect("refresh succeeds");
        context.descriptors().into_iter().map(|descriptor| descriptor.id).collect()
    }

    #[test]
    fn empty_section_when_disabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "alpha", "Do alpha.");
        let context = SkillsContext::new(manager(temp.path()), None, false, 4000);
        context.refresh().expect("refresh succeeds");
        assert_eq!(context.section(), "");
        assert!(context.descriptors().is_empty());
    }

    #[test]
    fn auto_load_discovers_all() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "zeta", "Do zeta.");
        write_toml_pack(temp.path(), "alpha", "Do alpha.");
        let context = SkillsContext::new(manager(temp.path()), None, true, 4000);
        let ids = refresh_ids(&context);
        assert_eq!(ids, vec!["alpha", "zeta"]);
        let section = context.section();
        assert!(section.contains("## Skills"), "section missing header: {section}");
        assert!(section.contains("Do alpha."));
        assert!(section.contains("Do zeta."));
        assert!(section.contains("### Skill alpha (alpha, v1.0.0)"));
    }

    #[test]
    fn subset_by_enabled_ids() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "alpha", "Do alpha.");
        write_toml_pack(temp.path(), "beta", "Do beta.");
        let context =
            SkillsContext::new(manager(temp.path()), Some(vec!["beta".to_string()]), true, 4000);
        let ids = refresh_ids(&context);
        assert_eq!(ids, vec!["beta"]);
        let section = context.section();
        assert!(section.contains("Do beta."));
        assert!(!section.contains("Do alpha."));
    }

    #[test]
    fn budget_truncation_appends_marker_within_budget() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "alpha", "A".repeat(2_000).as_str());
        write_toml_pack(temp.path(), "beta", "B".repeat(2_000).as_str());
        let context = SkillsContext::new(manager(temp.path()), None, true, 512);
        context.refresh().expect("refresh succeeds");
        let section = context.section();
        assert!(
            section.contains("[truncated: skill instructions exceed the session budget]"),
            "marker missing: {section}"
        );
        assert!(
            section.chars().count() <= 512,
            "section exceeds budget: {} chars",
            section.chars().count()
        );
    }

    #[test]
    fn marker_only_when_budget_smaller_than_marker() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "alpha", "Do alpha.");
        let context = SkillsContext::new(manager(temp.path()), None, true, 10);
        context.refresh().expect("refresh succeeds");
        let section = context.section();
        assert!(section.chars().count() <= 10);
        // The marker cannot fit in full, so the output is just the marker
        // truncated to the budget (starts with the marker's prefix).
        assert!(
            TRUNCATION_MARKER.starts_with(&section),
            "unexpected marker-only output: {section}"
        );
    }

    #[test]
    fn refresh_warns_and_skips_broken_pack_keeping_previous_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "alpha", "Do alpha.");
        let context = SkillsContext::new(manager(temp.path()), None, true, 4000);
        context.refresh().expect("first refresh succeeds");
        let before = context.section();
        assert!(before.contains("Do alpha."));

        // Introduce a malformed pack. Discovery is warn-and-continue: the
        // broken pack is logged at warning level and skipped (ADR-43 — the
        // skills manager contract), never aborting the rest of the scan, so
        // refresh succeeds and the previous section is kept.
        let bad_dir = temp.path().join("zz-broken");
        fs::create_dir_all(&bad_dir).expect("create bad pack dir");
        fs::write(bad_dir.join("skill.toml"), "id = [unclosed\n").expect("write bad manifest");

        assert!(context.refresh().is_ok(), "refresh must warn-and-continue past a broken pack");
        assert_eq!(context.section(), before, "skipped pack must not change the section");
        assert_eq!(context.descriptors().len(), 1);
    }

    #[test]
    fn set_enabled_ids_round_trips_through_refresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "alpha", "Do alpha.");
        write_toml_pack(temp.path(), "beta", "Do beta.");
        let context = SkillsContext::new(manager(temp.path()), None, false, 4000);
        assert!(context.section().is_empty(), "auto_load off with no ids -> empty");

        context.set_enabled_ids(Some(vec!["alpha".to_string()]));
        context.refresh().expect("refresh with alpha");
        let section = context.section();
        assert!(section.contains("Do alpha."));
        assert!(!section.contains("Do beta."));

        context.set_enabled_ids(None);
        context.refresh().expect("refresh with auto_load off");
        assert_eq!(context.section(), "", "None + auto_load off -> empty again");
    }

    #[test]
    fn disabled_configuration_performs_no_discovery() {
        // A context with an explicit empty allow-list and auto_load off must
        // not walk the filesystem: a missing search path is otherwise warned
        // about by discovery, which this test must not trigger.
        let context = SkillsContext::new(
            Arc::new(SkillManager::new(vec![std::path::PathBuf::from(
                "/definitely/not/a/skills/dir",
            )])),
            Some(Vec::new()),
            false,
            4000,
        );
        context.refresh().expect("refresh succeeds without touching the filesystem");
        assert_eq!(context.section(), "");
    }

    #[tokio::test]
    async fn report_injection_publishes_ids_sizes_and_no_content() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "alpha", "SECRET-ALPHA-CONTENT");
        write_toml_pack(temp.path(), "beta", "SECRET-BETA-CONTENT");
        let context = SkillsContext::new(manager(temp.path()), None, true, 4000);
        context.refresh().expect("refresh succeeds");
        let section = context.section();

        let bus = EventBus::default();
        let mut receiver = bus.subscribe();
        context.report_injection(&bus, Ulid::new());

        let event = receiver.recv().await.expect("skills injection event");
        match &event.kind {
            EventKind::SkillsInjected { skill_ids, total_chars, budget_chars } => {
                assert_eq!(skill_ids, &vec!["alpha".to_string(), "beta".to_string()]);
                assert_eq!(*total_chars, section.chars().count());
                assert_eq!(*budget_chars, 4000);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        // The audit record must never carry pack instruction content.
        let rendered = format!("{:?}", event.kind);
        assert!(!rendered.contains("SECRET-ALPHA-CONTENT"), "pack content leaked: {rendered}");
        assert!(!rendered.contains("SECRET-BETA-CONTENT"), "pack content leaked: {rendered}");
    }

    #[tokio::test]
    async fn report_injection_reflects_enabled_subset_and_budget() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_toml_pack(temp.path(), "alpha", "Do alpha.");
        write_toml_pack(temp.path(), "beta", "Do beta.");
        let context =
            SkillsContext::new(manager(temp.path()), Some(vec!["beta".to_string()]), true, 512);
        context.refresh().expect("refresh succeeds");

        let bus = EventBus::default();
        let mut receiver = bus.subscribe();
        context.report_injection(&bus, Ulid::new());

        let event = receiver.recv().await.expect("skills injection event");
        match &event.kind {
            EventKind::SkillsInjected { skill_ids, total_chars, budget_chars } => {
                assert_eq!(skill_ids, &vec!["beta".to_string()]);
                assert!(*total_chars > 0, "injected section must be non-empty");
                assert_eq!(*budget_chars, 512);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn report_injection_is_silent_when_no_skills_injected() {
        let context = SkillsContext::new(
            Arc::new(SkillManager::new(Vec::new())),
            Some(Vec::new()),
            false,
            4000,
        );
        context.refresh().expect("refresh succeeds");

        let bus = EventBus::default();
        let mut receiver = bus.subscribe();
        context.report_injection(&bus, Ulid::new());
        assert!(
            receiver.try_recv().is_err(),
            "no event must be published when nothing is injected"
        );
    }
}
