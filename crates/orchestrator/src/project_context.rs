//! Project AGENTS.md context injection (ADR-70).
//!
//! [`ProjectContext`] reads the user-global `AGENTS.md` (a config-supplied
//! path, or the platform default `~/.config/concerto/AGENTS.md`) plus the
//! per-project `<root>/AGENTS.md`, combines both into one bounded markdown
//! section, and serves that section to every prompt path (the single-agent
//! [`crate::prompts::PromptBuilder`] and the coordinator dispatch assembly).
//!
//! Unlike [`crate::skills_context::SkillsContext`] — which is process-scoped
//! and shared — the project context is **run-scoped**: it needs
//! `req.project_dir`, so the runtime constructs one instance per run and calls
//! [`ProjectContext::refresh`] once at startup. Reading
//! [`ProjectContext::section`] is cheap: it is formatted once in `refresh` and
//! cloned per call, so no filesystem work happens in the prompt hot path.
//!
//! The context is **fail-soft** by design (ADR-70): missing or empty files are
//! skipped silently, unreadable files are logged at debug and the previous
//! section is kept (with `Err` returned so the caller may decide), and a
//! disabled configuration performs no filesystem work — an unreadable
//! AGENTS.md can never crash the agent loop.
//!
//! Conflicts between the global and project files resolve project-over-global;
//! when both are present the section names both sources and states that
//! precedence explicitly so the model can disambiguate.
//!
//! The optional coordinator nudge ([`ProjectContext::nudge_frequency`]) is
//! advisory text only — the coordinator never edits AGENTS.md itself.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use tracing::debug;

/// Default budget (characters) for the injected project-context section when
/// `ProjectContextConfig.max_bytes` is unset. Mirrors
/// `docs/config.toml.example` (32 KiB per file).
pub const DEFAULT_PROJECT_CONTEXT_MAX_BYTES: usize = 32 * 1024;

/// Advisory note appended to the coordinator dispatch prompt when
/// `auto_update_agents_md` is on and the cadence is due (ADR-70 §6).
/// Text only — the coordinator never edits AGENTS.md itself; the note points
/// the orchestrated agent at the policy-gated filesystem tool when the
/// injected instructions are stale.
pub const PROJECT_CONTEXT_NUDGE: &str = "## Project context maintenance\nA project AGENTS.md is injected into these prompts. If its instructions became stale, refresh that file with the policy-gated filesystem tool (rewrite it in place). Do not edit anything else for this.";

/// Marker appended when a single source exceeds its share of the section
/// budget.
const TRUNCATION_MARKER: &str =
    "\n\n[truncated: AGENTS.md exceeds the project-context budget; raise max_bytes to include it]";

/// Header emitted only when at least one source is present.
const SECTION_HEADER: &str = "## Project context (AGENTS.md)";

/// Precedence line emitted only when both sources are present.
const PRECEDENCE_LINE: &str = "The project AGENTS.md overrides the global AGENTS.md on conflict.";

/// One injected source, ready to format.
#[derive(Debug)]
struct Source {
    /// Display path used as the `###` heading for this block.
    heading: String,
    /// Raw file content; truncated (with the marker, when needed) while the
    /// bounded section is assembled in [`format_section`].
    content: String,
}

/// Mutable state behind the context's read/write lock.
#[derive(Debug, Default)]
struct ProjectContextState {
    /// Whether the project `<root>/AGENTS.md` was read in the last refresh.
    /// Gates the coordinator nudge — never nudge a phantom file.
    project_present: bool,
    /// The formatted, budgeted section served to prompts.
    section: String,
}

/// Run-scoped handle to the injected project-context section (ADR-70).
///
/// Constructed once per run ([`ProjectContext::from_config`]) and consumed by
/// both prompt paths. The constructor performs no filesystem work; call
/// [`ProjectContext::refresh`] once at startup.
#[derive(Debug)]
pub struct ProjectContext {
    /// Whether the feature is on. A disabled context stores no paths of
    /// consequence, performs no filesystem work in [`ProjectContext::refresh`],
    /// and yields an empty [`ProjectContext::section`].
    enabled: bool,
    project_dir: PathBuf,
    global_path: Option<PathBuf>,
    max_bytes: usize,
    nudge_frequency: Option<u64>,
    state: RwLock<ProjectContextState>,
}

impl Default for ProjectContext {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ProjectContext {
    /// Build a run-scoped context from config plus the run's project directory.
    ///
    /// `None` — or a config with `enabled = false` — yields the fully disabled
    /// context: no filesystem work and an empty section. `global_path` falls
    /// back to the platform default (`~/.config/concerto/AGENTS.md` on POSIX)
    /// when unset; `max_bytes` falls back to
    /// [`DEFAULT_PROJECT_CONTEXT_MAX_BYTES`] when unset. No filesystem work
    /// happens here; call [`ProjectContext::refresh`] once at startup.
    pub fn from_config(
        config: Option<&concerto_config::ProjectContextConfig>,
        project_dir: &Path,
    ) -> Self {
        let Some(config) = config.filter(|config| config.enabled) else {
            return Self::disabled();
        };
        Self::new(
            project_dir.to_path_buf(),
            config.resolved_global_path(),
            config.max_bytes.unwrap_or(DEFAULT_PROJECT_CONTEXT_MAX_BYTES),
            config.nudge_frequency(),
        )
    }

    /// Direct constructor (tests and embedders). `nudge_frequency` is the
    /// resolved cadence from config; [`ProjectContext::nudge_frequency`]
    /// additionally requires the project AGENTS.md to be present.
    pub fn new(
        project_dir: PathBuf,
        global_path: Option<PathBuf>,
        max_bytes: usize,
        nudge_frequency: Option<u64>,
    ) -> Self {
        Self {
            enabled: true,
            project_dir,
            global_path,
            max_bytes,
            nudge_frequency,
            state: RwLock::new(ProjectContextState::default()),
        }
    }

    /// A disabled context by construction: empty section, no I/O, no nudge.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            project_dir: PathBuf::new(),
            global_path: None,
            max_bytes: DEFAULT_PROJECT_CONTEXT_MAX_BYTES,
            nudge_frequency: None,
            state: RwLock::new(ProjectContextState::default()),
        }
    }

    /// Re-read both AGENTS.md sources and reformat the bounded section.
    ///
    /// Fail-soft contract (ADR-70):
    /// - a source that is absent or empty is skipped silently;
    /// - an unreadable source logs at debug, keeps the previous state, and
    ///   surfaces `Err` so the caller may decide (the loop must never crash
    ///   on it);
    /// - a disabled configuration performs no filesystem work.
    pub fn refresh(&self) -> std::io::Result<()> {
        // A disabled context performs no filesystem work at all.
        if !self.enabled {
            return Ok(());
        }
        let global = self.read_source(self.global_path.as_deref());
        let project = self.read_source(Some(&self.project_dir.join("AGENTS.md")));
        let (global, project) = match (global, project) {
            (Ok(global), Ok(project)) => (global, project),
            (Err(global_error), _) => {
                debug!(error = %global_error, "global AGENTS.md unreadable; keeping previous section");
                return Err(global_error);
            }
            (_, Err(project_error)) => {
                debug!(error = %project_error, "project AGENTS.md unreadable; keeping previous section");
                return Err(project_error);
            }
        };

        let section = format_section(global.as_ref(), project.as_ref(), self.max_bytes);
        let mut state = self.state.write().unwrap_or_else(|poison| poison.into_inner());
        state.project_present = project.is_some();
        state.section = section;
        Ok(())
    }

    /// Read one AGENTS.md source, keeping the raw content for later (the
    /// bounded, marker-aware truncation happens while the section is
    /// assembled, so the whole section — headers included — stays within
    /// `max_bytes`).
    ///
    /// * `Ok(None)` — path unset, or the file is absent/empty (skipped
    ///   silently).
    /// * `Ok(Some(..))` — raw content (possibly truncated later with a
    ///   marker).
    /// * `Err(..)` — the file exists but could not be read.
    fn read_source(&self, path: Option<&Path>) -> std::io::Result<Option<Source>> {
        let Some(path) = path else {
            return Ok(None);
        };
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut raw = String::new();
        file.read_to_string(&mut raw)?;
        if raw.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(Source { heading: path.to_string_lossy().into_owned(), content: raw }))
    }

    /// The formatted project-context section for this run. Empty when disabled
    /// or when no source is present. Cheap (one `String` clone); no I/O per
    /// prompt build.
    pub fn section(&self) -> String {
        if !self.enabled {
            return String::new();
        }
        self.state.read().unwrap_or_else(|poison| poison.into_inner()).section.clone()
    }

    /// Whether the project `<root>/AGENTS.md` is currently being injected.
    pub fn project_present(&self) -> bool {
        self.state.read().unwrap_or_else(|poison| poison.into_inner()).project_present
    }

    /// The coordinator nudge cadence: the configured frequency only when the
    /// whole feature is on, `auto_update_agents_md` is opted in, AND the
    /// project AGENTS.md is present — otherwise `None`. Advisory text only.
    pub fn nudge_frequency(&self) -> Option<u64> {
        self.nudge_frequency.filter(|_| self.project_present())
    }
}

/// Assemble the injected section from both sources within the total budget
/// (documented contract):
///
/// 1. Emit nothing when neither source is present, so a disabled or empty
///    configuration adds no `## Project context` header to prompts.
/// 2. Emit the header, then — only when both sources are present — the
///    precedence line stating that the project AGENTS.md overrides the global
///    one on conflict.
/// 3. One `### <path>` block per present source, global first, project last.
///    The budget covers the whole section: the framing (header, precedence
///    line, per-source headings) is subtracted first, and the remaining
///    budget is shared between the two contents — each truncated with the
///    marker when it would not fit — so the result never exceeds `max_bytes`
///    characters.
fn format_section(global: Option<&Source>, project: Option<&Source>, max_bytes: usize) -> String {
    if global.is_none() && project.is_none() {
        return String::new();
    }

    // Chars consumed by the section framing regardless of content length:
    // "\n### " (5) + heading + "\n" (1) per present source.
    let mut static_len = SECTION_HEADER.chars().count();
    if global.is_some() && project.is_some() {
        static_len += 1 + PRECEDENCE_LINE.chars().count();
    }
    for source in [global, project].into_iter().flatten() {
        static_len += 5 + source.heading.chars().count() + 1;
    }

    // Remaining budget shared between the contents. When both are present,
    // split it in half (global takes the odd remainder) so neither starves
    // the other and the section stays within `max_bytes`.
    let content_budget = max_bytes.saturating_sub(static_len);
    let (global_budget, project_budget) = if global.is_some() && project.is_some() {
        (content_budget / 2 + content_budget % 2, content_budget / 2)
    } else {
        (content_budget, content_budget)
    };

    let mut out = String::from(SECTION_HEADER);
    if global.is_some() && project.is_some() {
        out.push('\n');
        out.push_str(PRECEDENCE_LINE);
    }
    if let Some(global) = global {
        out.push_str("\n### ");
        out.push_str(&global.heading);
        out.push('\n');
        out.push_str(&truncate_body(&global.content, global_budget));
    }
    if let Some(project) = project {
        out.push_str("\n### ");
        out.push_str(&project.heading);
        out.push('\n');
        out.push_str(&truncate_body(&project.content, project_budget));
    }

    // Last-resort guard for pathological budgets where the framing alone
    // exceeds `max_bytes`: keep the section character-boundary-safe and within
    // budget (fail-soft, mirroring `truncate_chars`).
    if out.chars().count() > max_bytes {
        out = truncate_chars(&out, max_bytes);
    }
    out
}

/// Truncate a single source to its share of the budget (characters), reserving
/// the marker up front and appending it when content is cut. The result is
/// always `<= max_bytes` characters and mirrors the skills `format_section`
/// marker reservation.
fn truncate_body(content: &str, max_bytes: usize) -> String {
    let marker_chars = TRUNCATION_MARKER.chars().count();
    if max_bytes < marker_chars {
        // The marker itself cannot fit; emit only the marker (truncated).
        return truncate_chars(TRUNCATION_MARKER, max_bytes);
    }
    let budget = max_bytes - marker_chars;
    let body = truncate_chars(content, budget);
    if body == content {
        return content.to_string();
    }
    let mut out = body;
    out.push_str(TRUNCATION_MARKER);
    out
}

/// First `max_chars` characters of `text` (character-boundary safe).
fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::ProjectContextConfig;
    use std::fs;

    fn write(root: &Path, name: &str, content: &str) {
        fs::create_dir_all(root).expect("create dir");
        fs::write(root.join(name), content).expect("write file");
    }

    #[test]
    fn disabled_yields_empty_section() {
        let context = ProjectContext::disabled();
        context.refresh().expect("disabled refresh is a no-op");
        assert_eq!(context.section(), "");
        assert_eq!(context.nudge_frequency(), None);
        assert!(!context.project_present());
    }

    #[test]
    fn none_config_disables_feature() {
        let temp = tempfile::tempdir().expect("tempdir");
        write(temp.path(), "AGENTS.md", "Project rules.");
        let context = ProjectContext::from_config(None, temp.path());
        context.refresh().expect("refresh succeeds");
        assert_eq!(context.section(), "", "None config keeps the feature off");
    }

    #[test]
    fn disabled_config_performs_no_io() {
        let bad = PathBuf::from("/definitely/not/readable-AGENTS.md");
        let cfg = ProjectContextConfig {
            enabled: false,
            global_path: Some(bad.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let context = ProjectContext::from_config(Some(&cfg), Path::new("/tmp/irrelevant"));
        context.refresh().expect("disabled refresh performs no filesystem work");
        assert_eq!(context.section(), "");
        assert_eq!(context.nudge_frequency(), None);
    }

    #[test]
    fn project_only_section() {
        let temp = tempfile::tempdir().expect("tempdir");
        write(temp.path(), "AGENTS.md", "Project rules.");
        let context = ProjectContext::new(temp.path().to_path_buf(), None, 4000, None);
        context.refresh().expect("refresh succeeds");
        let section = context.section();
        assert!(section.contains("## Project context (AGENTS.md)"), "header missing: {section}");
        assert!(section.contains("Project rules."));
        assert!(section.contains("\n### "), "no per-source heading: {section}");
        assert!(
            !section.contains("overrides the global"),
            "no precedence line with a single source: {section}"
        );
        assert!(context.project_present());
    }

    #[test]
    fn global_only_section() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join("global-AGENTS.md");
        write(temp.path(), "global-AGENTS.md", "Global rules.");
        let context = ProjectContext::new(temp.path().to_path_buf(), Some(global), 4000, None);
        context.refresh().expect("refresh succeeds");
        let section = context.section();
        assert!(section.contains("Global rules."), "global content missing: {section}");
        assert!(!section.contains("Project rules."), "no project file was written");
        assert!(!context.project_present());
    }

    #[test]
    fn both_present_names_sources_and_orders_global_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join("global-AGENTS.md");
        write(temp.path(), "global-AGENTS.md", "Global rules.");
        write(temp.path(), "AGENTS.md", "Project rules.");
        let context = ProjectContext::new(temp.path().to_path_buf(), Some(global), 4000, None);
        context.refresh().expect("refresh succeeds");
        let section = context.section();
        assert!(section.contains("## Project context (AGENTS.md)"), "header missing: {section}");
        assert!(
            section.contains("overrides the global"),
            "precedence line missing with both sources: {section}"
        );
        let global_index = section.find("Global rules.").expect("global content present");
        let project_index = section.find("Project rules.").expect("project content present");
        assert!(
            global_index < project_index,
            "global block must precede the project block: {section}"
        );
        assert!(context.project_present());
    }

    #[test]
    fn missing_files_are_skipped_silently() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Neither the project dir nor the global path contains a file.
        let context = ProjectContext::new(
            temp.path().join("no-such-project"),
            Some(temp.path().join("no-such-global-AGENTS.md")),
            4000,
            None,
        );
        context.refresh().expect("missing files are not errors");
        assert_eq!(context.section(), "");
        assert!(!context.project_present());
    }

    #[test]
    fn per_file_truncation_appends_marker_within_budget() {
        let temp = tempfile::tempdir().expect("tempdir");
        let long = "A".repeat(2_000);
        write(temp.path(), "AGENTS.md", &long);
        let context = ProjectContext::new(temp.path().to_path_buf(), None, 512, None);
        context.refresh().expect("refresh succeeds");
        let section = context.section();
        assert!(
            section.contains("[truncated: AGENTS.md exceeds the project-context budget"),
            "marker missing: {section}"
        );
        assert!(
            section.chars().count() <= 512,
            "section exceeds budget: {} chars",
            section.chars().count()
        );
    }

    #[test]
    fn unreadable_source_keeps_previous_state_and_returns_err() {
        let temp = tempfile::tempdir().expect("tempdir");
        write(temp.path(), "AGENTS.md", "Project rules.");
        let context = ProjectContext::new(temp.path().to_path_buf(), None, 4000, None);
        context.refresh().expect("first refresh succeeds");
        let before = context.section();
        assert!(!before.is_empty(), "project file was injected");

        // Replace the project file with a directory so it becomes unreadable.
        let file = temp.path().join("AGENTS.md");
        fs::remove_file(&file).expect("remove file");
        fs::create_dir(&file).expect("replace with directory");
        assert!(context.refresh().is_err(), "unreadable source must surface Err");
        assert_eq!(context.section(), before, "failed refresh keeps the previous section");
    }

    #[test]
    fn nudge_requires_project_file_and_opt_in() {
        let temp = tempfile::tempdir().expect("tempdir");
        write(temp.path(), "AGENTS.md", "Project rules.");

        // Opted in with a project file -> Some(frequency).
        let cfg = ProjectContextConfig {
            enabled: true,
            auto_update_agents_md: true,
            update_frequency: 3,
            ..Default::default()
        };
        let context = ProjectContext::from_config(Some(&cfg), temp.path());
        context.refresh().expect("refresh succeeds");
        assert_eq!(context.nudge_frequency(), Some(3));

        // Same config but no project AGENTS.md -> never nudge a phantom file.
        let empty = tempfile::tempdir().expect("tempdir");
        let phantom = ProjectContext::from_config(Some(&cfg), empty.path());
        phantom.refresh().expect("refresh succeeds");
        assert_eq!(phantom.nudge_frequency(), None);

        // Feature on but the nudge not opted in -> None even with a file.
        let off = ProjectContextConfig { enabled: true, ..Default::default() };
        let opted_out = ProjectContext::from_config(Some(&off), temp.path());
        opted_out.refresh().expect("refresh succeeds");
        assert_eq!(opted_out.nudge_frequency(), None);
    }
}
