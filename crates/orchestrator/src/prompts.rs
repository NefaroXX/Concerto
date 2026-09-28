//! Prompt builder — assembles `CompletionRequest` instances from task,
//! working memory, conversation history, and session summary.
//!
//! Every completion call in the orchestrator goes through this builder,
//! ensuring consistent injection of the working memory block, system
//! prompt, and previous session summary.

use std::sync::Arc;
use std::time::Duration;

use concerto_config::{ShellBackendType, ShellProfileConfig};
use concerto_core::error::ProviderError;
use concerto_core::event::EventBus;
use concerto_core::ids::Ulid;
use concerto_core::text::normalize_typographic;
use concerto_core::traits::provider::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, CompletionUsage, Message, Role, ToolCall};
use concerto_core::{CancellationToken, OrchestratorError, TaskId};
use concerto_providers::retry::{with_provider_retry, RetryPolicy};

use crate::project_context::ProjectContext;
use crate::skills_context::SkillsContext;

/// The `{working_memory}` placeholder as the shipped templates spell it —
/// including the blank-line separator that precedes it. Non-empty working
/// memory is substituted in place; an empty block removes the separator and
/// the placeholder together (see [`PromptBuilder::assemble_system`]).
///
/// Shared with the coordinator dispatch prompt, which appends the same
/// placeholder so the multi-agent prompt follows the identical
/// working-memory assembly path as the single-agent loop.
pub(crate) const WORKING_MEMORY_SEPARATOR_PLACEHOLDER: &str = "\n\n{working_memory}";

/// Builds the full `CompletionRequest` for each agent cycle.
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    /// The system prompt template. `{working_memory}` and `{summary}`
    /// placeholders are replaced at build time. The shipped
    /// `SYSTEM_PROMPT_*` templates all carry `{working_memory}`, so the
    /// active-state + retrieved-chunks block reaches the model on the
    /// default assembly path — not only under `cache_stable_prefix`.
    system_template: String,
    /// Runtime-owned skills context (ADR-43, Task 4). When set and non-empty,
    /// the current skills section is appended to the system prompt on every
    /// build, so a live refresh takes effect without rebuilding the builder.
    skills: Option<Arc<SkillsContext>>,
    /// Run-scoped project AGENTS.md context (ADR-70). When set and non-empty,
    /// the AGENTS section is appended after the skills section on every build.
    project_context: Option<Arc<ProjectContext>>,
    /// The session's selected shell profile (ADR-28). Rendered into the
    /// OS/shell identity card appended to every built system prompt.
    /// `None` renders the card from OS facts plus the detected OS default
    /// shell instead — the card is never omitted and never errors.
    shell_profile: Option<ShellProfileConfig>,
    /// ADR-048 prefix discipline (the `[context].cache_stable_prefix` knob).
    ///
    /// `false` (default) keeps today's byte-identical assembly. `true` pins a
    /// byte-stable head — template, resolved summary, skills block, project
    /// AGENTS.md context and environment card — and appends the volatile
    /// working-memory/retrieved block *after* it, still inside the single
    /// system message, so a prompt cache keyed on the head survives a churned
    /// tail. See [`PromptBuilder::stable_system_head`].
    cache_stable_prefix: bool,
}

impl PromptBuilder {
    /// Create a new builder with the given system prompt template and no
    /// skills context.
    pub fn new(system_template: impl Into<String>) -> Self {
        Self {
            system_template: system_template.into(),
            skills: None,
            project_context: None,
            shell_profile: None,
            cache_stable_prefix: false,
        }
    }

    /// Create a builder that appends the enabled skills section to the system
    /// prompt. Pass `None` to keep the plain `new` behavior.
    pub fn with_skills(
        system_template: impl Into<String>,
        skills: Option<Arc<SkillsContext>>,
    ) -> Self {
        Self {
            system_template: system_template.into(),
            skills,
            project_context: None,
            shell_profile: None,
            cache_stable_prefix: false,
        }
    }

    /// Attach the run-scoped project AGENTS.md context (ADR-70). When present
    /// its section is appended after the skills section on every build. Pass
    /// `None` to keep the current behavior.
    pub fn with_project_context(mut self, project_context: Option<Arc<ProjectContext>>) -> Self {
        self.project_context = project_context;
        self
    }

    /// Attach the session's selected shell profile for the identity card.
    /// Pass `None` to fall back to the OS-only card (OS facts plus the
    /// detected OS default shell).
    pub fn with_shell_profile(mut self, profile: Option<ShellProfileConfig>) -> Self {
        self.shell_profile = profile;
        self
    }

    /// Enable ADR-048 prefix discipline (the `[context].cache_stable_prefix`
    /// knob). Defaults to `false`, which keeps [`PromptBuilder::build`]
    /// byte-identical to today's assembly.
    pub fn with_cache_stable_prefix(mut self, enabled: bool) -> Self {
        self.cache_stable_prefix = enabled;
        self
    }

    /// Whether this builder pins a byte-stable system head (ADR-048).
    pub fn cache_stable_prefix(&self) -> bool {
        self.cache_stable_prefix
    }

    /// Assemble the complete system message: the template with `{summary}`
    /// and `{working_memory}` resolved, then the skills block, the project
    /// AGENTS.md context and the environment card, in that order.
    ///
    /// `working_memory` is the only volatile member; everything substituted
    /// into the template from `prev_summary` and everything appended after it
    /// is session-stable within a run.
    fn assemble_system(&self, working_memory: &str, prev_summary: Option<&str>) -> String {
        let mut system = self.system_template.clone();

        if let Some(summary) = prev_summary {
            system = system.replace("{summary}", summary);
        } else {
            system = system.replace("{summary}", "");
        }

        // Working memory (ADR-48 §3/§4): the templates ask for the volatile
        // active-state + retrieved-chunks block with a trailing
        // `{working_memory}` placeholder preceded by a blank line. An empty
        // block removes the separator as well, so an empty state leaves the
        // template byte-identical — no dangling blank line, no placeholder
        // leak, no fabricated section header.
        if working_memory.is_empty() {
            system = system.replace(WORKING_MEMORY_SEPARATOR_PLACEHOLDER, "");
            system = system.replace("{working_memory}", "");
        } else {
            system = system.replace("{working_memory}", working_memory);
        }

        // Append the skills section after placeholder substitution so skill
        // instructions can never collide with template placeholders. The
        // section is already formatted and budgeted by `SkillsContext`.
        if let Some(skills) = &self.skills {
            let section = skills.section();
            if !section.is_empty() {
                system.push_str("\n\n");
                system.push_str(&section);
            }
        }

        // Project AGENTS.md context (ADR-70): injected between the skills
        // section and the identity card, matching the coordinator's assembly
        // order (skills -> AGENTS -> environment card). Already formatted and
        // per-file budgeted by `ProjectContext`.
        if let Some(project_context) = &self.project_context {
            let section = project_context.section();
            if !section.is_empty() {
                system.push_str("\n\n");
                system.push_str(&section);
            }
        }

        // OS/shell identity card (custom-ai-shell plan, Phase C): grounds
        // every prompt in the host facts and the selected agent shell's
        // dialect gotchas. Always appended — with no configured profile it
        // falls back to OS facts plus the detected OS default shell, so the
        // card is never empty and never errors.
        let card = environment_card(self.shell_profile.as_ref());
        system.push_str("\n\n");
        system.push_str(&card);

        system
    }

    /// ADR-048: the byte-stable head of the system message — the assembled
    /// system message with the volatile working memory blanked out.
    ///
    /// Every byte this returns is built from facts that do not change within
    /// a run: the template, the resolved `{summary}`, the skills block (sorted
    /// and deduplicated by [`SkillsContext`]), the project AGENTS.md context
    /// and the environment card. Tool schemas never enter this string at all —
    /// they ride `CompletionRequest::tools`, rendered from the executor
    /// registry in registration order — so they are stable for the same
    /// reason.
    ///
    /// With [`PromptBuilder::with_cache_stable_prefix`] enabled,
    /// [`PromptBuilder::build`] emits exactly these bytes first and appends
    /// the volatile working memory (active state plus retrieved chunks) after
    /// them, inside the same single system message so last-system-wins
    /// adapters (Anthropic, Gemini) still see it while the bytes before it
    /// never move.
    pub fn stable_system_head(&self, prev_summary: Option<&str>) -> String {
        self.assemble_system("", prev_summary)
    }

    /// Build a `CompletionRequest` from the current context.
    ///
    /// * `working_memory_block` — the XML block from `WorkingMemory::to_system_block()`.
    /// * `messages` — the conversation history (short-term memory messages).
    /// * `prev_summary` — optional summary from a previous session.
    /// * `tools` — optional tool definitions to include.
    pub fn build(
        &self,
        working_memory_block: &str,
        messages: &[Message],
        prev_summary: Option<&str>,
        tools: Option<&[concerto_core::types::ToolDefinition]>,
    ) -> CompletionRequest {
        let system = if self.cache_stable_prefix {
            // ADR-048: pin the stable head, then append the volatile tail.
            // The tail lands after the environment card and still inside the
            // one system message — adapters that keep only the last system
            // message see the same content, but the bytes above the tail are
            // identical from build to build.
            let mut system = self.stable_system_head(prev_summary);
            if !working_memory_block.is_empty() {
                system.push_str("\n\n");
                system.push_str(working_memory_block);
            }
            system
        } else {
            // Default path: the working memory is substituted wherever the
            // template asks for it — the shipped `SYSTEM_PROMPT_*` templates
            // all carry `{working_memory}`, so the block is delivered here
            // too (previously it was only delivered under
            // `cache_stable_prefix`). A template without the placeholder
            // still drops it entirely.
            self.assemble_system(working_memory_block, prev_summary)
        };

        let mut all_messages = Vec::with_capacity(messages.len() + 1);

        all_messages.push(Message {
            role: Role::System,
            content: system,
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        });

        all_messages.extend_from_slice(messages);

        CompletionRequest {
            model: String::new(), // filled in by the caller or provider context guard
            messages: all_messages,
            tools: tools.map(|t| t.to_vec()),
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            stream: true,
        }
    }
}

/// The shell dialect family an identity card curates gotchas for.
///
/// Detection is deliberately cheap: the executable's file stem only. No
/// process is spawned and no filesystem probe runs — `version_string()` is
/// never called here because the card ships into every prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellDialect {
    /// POSIX-style shells: bash, sh, zsh, dash, ash.
    Posix,
    /// PowerShell (pwsh / Windows PowerShell).
    PowerShell,
    /// Windows cmd.exe.
    Cmd,
    /// Unknown family — keep the guidance shell-agnostic.
    Generic,
}

impl ShellDialect {
    /// Classify a dialect from an executable path or bare name.
    fn from_executable(executable: &str) -> Self {
        match executable_stem(executable).as_str() {
            "bash" | "sh" | "zsh" | "dash" | "ash" => Self::Posix,
            "pwsh" | "powershell" => Self::PowerShell,
            "cmd" => Self::Cmd,
            _ => Self::Generic,
        }
    }

    /// Short label used in the card's dialect-notes header.
    fn label(self) -> &'static str {
        match self {
            Self::Posix => "posix",
            Self::PowerShell => "powershell",
            Self::Cmd => "cmd",
            Self::Generic => "generic",
        }
    }

    /// Curated gotchas for the detected family only — this text ships into
    /// every prompt, so it stays short.
    fn notes(self) -> &'static [&'static str] {
        match self {
            Self::Posix => &[
                "Quote every `$VAR` expansion; unquoted expansions split on whitespace.",
                "Prefer `printf` over `echo -e`/`echo -n` for escape sequences and exact output.",
                "Each call is a fresh one-shot shell: no state carries over and `set -o pipefail` is per-invocation.",
            ],
            Self::PowerShell => &[
                "There is no `&&`/`||`; separate commands with `;` or run one command per call.",
                "`curl`/`ls` are aliases for cmdlets with different flags — prefer full cmdlet names (`Invoke-WebRequest`, `Get-ChildItem`) and prefix native executables with `&`.",
                "Native executables set `$LASTEXITCODE`; cmdlets set `$?` — check the right one.",
            ],
            Self::Cmd => &[
                "Variables expand as `%VAR%` (`!VAR!` only with delayed expansion).",
                "No `ls`/`grep`/`curl` by default — use `dir`, `findstr`, and `curl.exe`.",
                "Quote paths with spaces; `/C` parsing treats `^` as the escape character.",
            ],
            Self::Generic => &[
                "Use the simplest single command; avoid chaining operators (`&&`, `;`, `|`) and shell-specific syntax.",
            ],
        }
    }
}

/// File-name label for a configured executable (`/usr/bin/bash` → `bash`,
/// `pwsh.exe` → `pwsh.exe`); `unspecified` when the field is blank. Splits on
/// both separators so a Windows-style path still yields its file name.
fn executable_label(executable: &str) -> String {
    let trimmed = executable.trim();
    if trimmed.is_empty() {
        return "unspecified".to_string();
    }
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).to_string()
}

/// Lowercase file stem used for dialect detection (`pwsh.exe` → `pwsh`).
fn executable_stem(executable: &str) -> String {
    let label = executable_label(executable);
    match label.split_once('.') {
        Some((stem, _)) => stem.to_ascii_lowercase(),
        None => label.to_ascii_lowercase(),
    }
}

/// Backend label for the card's shell line.
fn backend_label(backend: ShellBackendType) -> &'static str {
    match backend {
        ShellBackendType::System => "system",
        ShellBackendType::Managed => "managed",
        ShellBackendType::Custom => "custom",
        _ => "system",
    }
}

/// Render the OS/shell identity card appended to agent system prompts
/// (custom-ai-shell plan, Phase C).
///
/// The card is compact (< 20 lines) and deterministic: OS + arch, the
/// canonical agent shell (profile id, executable file name, backend), and
/// 2–3 dialect gotchas curated for the detected shell family only.
///
/// * `Some(profile)` — facts come from the selected ADR-28 shell profile.
/// * `None` — falls back to OS facts plus `detect_os_default_shell()`; the
///   dialect notes then reflect the detected default shell's family. The
///   card is never empty and never errors, and it never probes shell
///   versions in the hot path (no process is spawned).
pub fn environment_card(profile: Option<&ShellProfileConfig>) -> String {
    let (shell_line, dialect) = match profile {
        Some(profile) => {
            let label = executable_label(&profile.executable);
            (
                format!(
                    "profile `{}` — {label} (backend: {})",
                    profile.id,
                    backend_label(profile.backend)
                ),
                ShellDialect::from_executable(&profile.executable),
            )
        }
        None => {
            let detected = concerto_tools::shell::detect_os_default_shell();
            (
                format!("OS default (`{}`)", executable_label(&detected)),
                ShellDialect::from_executable(&detected),
            )
        }
    };

    let mut card = format!(
        "## Environment\n- OS: {} ({})\n- Agent shell: {}\n- Shell dialect notes ({}):",
        std::env::consts::OS,
        std::env::consts::ARCH,
        shell_line,
        dialect.label()
    );
    for note in dialect.notes() {
        card.push_str("\n  - ");
        card.push_str(note);
    }
    for note in platform_notes(std::env::consts::OS) {
        card.push_str("\n  - ");
        card.push_str(&note);
    }
    card
}

/// Per-OS gotchas appended after the shell-dialect notes. POSIX systems get
/// no extra lines (the dialect notes already cover them); Windows warns that
/// `/tmp` does not exist.
fn platform_notes(os: &str) -> Vec<String> {
    match os {
        "windows" => {
            vec!["No `/tmp`: scratch files go in `%TEMP%` or the project directory, never `/tmp`."
                .to_string()]
        }
        _ => Vec::new(),
    }
}

/// Parse the first valid JSON value from a model response.
///
/// The parser accepts strict JSON, fenced JSON, and JSON surrounded by prose.
/// Candidate boundaries are scanned with string/escape awareness instead of
/// pairing the first opening brace with the final closing brace, which was the
/// source of repeated architect failures when a model added commentary.
///
/// Typographic (Unicode) punctuation in model output is normalized to ASCII
/// first — models emit curly quotes, en dashes, and non-breaking hyphens that
/// `serde_json` rejects outright.
pub fn parse_json_value(text: &str) -> Option<serde_json::Value> {
    let normalized = normalize_typographic(text);
    let trimmed = normalized.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Some(value);
    }

    let unfenced = strip_outer_fence(trimmed);
    if unfenced != trimmed {
        if let Ok(value) = serde_json::from_str(unfenced) {
            return Some(value);
        }
    }

    parse_balanced_candidate(unfenced).or_else(|| parse_balanced_candidate(trimmed))
}

/// Deserialize the first JSON fragment in a model response that deserializes
/// to `T`.
///
/// After normalizing typographic punctuation, tries in order: strict
/// whole-text deserialization, deserialization after stripping an outer code
/// fence, then every balanced `{...}`/`[...]` candidate in occurrence order.
/// A prose prefix may contain an earlier balanced fragment that is valid JSON
/// of the wrong type — each candidate is tried until one deserializes to `T`.
pub fn parse_json_substring<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    let normalized = normalize_typographic(text);
    let trimmed = normalized.trim();

    if let Ok(value) = serde_json::from_str::<T>(trimmed) {
        return Some(value);
    }

    let unfenced = strip_outer_fence(trimmed);
    if unfenced != trimmed {
        if let Ok(value) = serde_json::from_str::<T>(unfenced) {
            return Some(value);
        }
    }

    for candidate in balanced_candidates(trimmed) {
        if let Ok(value) = serde_json::from_str::<T>(candidate) {
            return Some(value);
        }
    }
    None
}

fn strip_outer_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some(first_newline) = rest.find('\n') else {
        return text;
    };
    let body = rest[first_newline + 1..].trim_end();
    body.strip_suffix("```").map_or(text, str::trim)
}

fn parse_balanced_candidate(text: &str) -> Option<serde_json::Value> {
    for (start, opening) in text.char_indices().filter(|(_, ch)| matches!(ch, '{' | '[')) {
        let Some(end) = balanced_end(text, start, opening) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str(&text[start..end]) {
            return Some(value);
        }
    }
    None
}

/// Yield every complete balanced `{...}` / `[...]` span of `text` in
/// occurrence order, scanning with the string/escape awareness of
/// [`balanced_end`].
///
/// Unlike [`parse_balanced_candidate`], which stops at the first span that
/// parses as *any* JSON, this yields all spans so a prose prefix containing an
/// earlier JSON fragment of the wrong type does not hide the real payload —
/// callers deserialize each candidate against their concrete target type.
fn balanced_candidates(text: &str) -> impl Iterator<Item = &str> {
    text.char_indices().filter(|(_, ch)| matches!(ch, '{' | '[')).filter_map(
        move |(start, opening)| {
            let end = balanced_end(text, start, opening)?;
            Some(&text[start..end])
        },
    )
}

fn balanced_end(text: &str, start: usize, opening: char) -> Option<usize> {
    let mut stack = vec![opening];
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in text[start + opening.len_utf8()..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' | '[' => stack.push(ch),
            '}' if stack.pop() != Some('{') => return None,
            ']' if stack.pop() != Some('[') => return None,
            '}' | ']' => {}
            _ => {}
        }

        if stack.is_empty() {
            return Some(start + opening.len_utf8() + offset + ch.len_utf8());
        }
    }
    None
}

/// Collect a `CompletionStream` into its full text, any reasoning text, any
/// tool calls, and the provider-reported usage (ADR-48 §4).
///
/// Returns `(text, reasoning, tool_calls, usage)`. `reasoning` is `None` when
/// no streamed reasoning was observed (ADR-46); otherwise the concatenated
/// reasoning deltas. `usage` is `Some` only when the terminal chunk carried a
/// provider usage report (providers report usage exclusively on the final
/// chunk).
pub async fn collect_stream(
    mut stream: CompletionStream,
) -> Result<(String, Option<String>, Vec<ToolCall>, Option<CompletionUsage>), OrchestratorError> {
    use futures::StreamExt;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(OrchestratorError::Provider)?;
        text.push_str(&chunk.delta);
        if let Some(r) = chunk.reasoning {
            reasoning.push_str(&r);
        }
        if let Some(tool_call) = chunk.tool_call {
            tool_calls.push(tool_call);
        }
        if chunk.is_final {
            usage = chunk.usage;
        }
    }
    let reasoning = if reasoning.is_empty() { None } else { Some(reasoning) };
    Ok((text, reasoning, tool_calls, usage))
}

/// Execute and collect one logical provider request through the single retry
/// boundary used by every orchestration path.
///
/// The header/stream-creation deadline and the between-chunk idle deadline are
/// separate so a long response remains valid while it continues producing
/// data. A retry recreates only this request; it never replays an agent or any
/// tool side effects.
///
/// Returns `(text, reasoning, tool_calls, usage)`; see [`collect_stream`].
#[allow(clippy::too_many_arguments)]
pub async fn complete_provider_request(
    provider: &std::sync::Arc<dyn LlmProvider>,
    request: &CompletionRequest,
    retry_policy: &RetryPolicy,
    bus: &EventBus,
    session_id: Ulid,
    task_id: TaskId,
    cancel: &CancellationToken,
) -> Result<(String, Option<String>, Vec<ToolCall>, Option<CompletionUsage>), OrchestratorError> {
    let first_byte_timeout = Duration::from_secs(retry_policy.config().time_to_first_byte_seconds);
    let idle_timeout = Duration::from_secs(retry_policy.config().stream_idle_timeout_seconds);
    let provider_name = provider.provider_name();

    with_provider_retry(retry_policy, bus, session_id, task_id, provider_name, cancel, || {
        let provider = provider.clone();
        let request = request.clone();
        let request_cancel = cancel.clone();
        async move {
            let stream = tokio::time::timeout(
                first_byte_timeout,
                provider.stream_completion(request, request_cancel.clone()),
            )
            .await
            .map_err(|_| ProviderError::Timeout {
                phase: "time-to-first-byte",
                timeout: first_byte_timeout,
            })??;

            collect_stream_with_timeouts(stream, &request_cancel, first_byte_timeout, idle_timeout)
                .await
        }
    })
    .await
    .map_err(|error| match error {
        ProviderError::Cancelled => OrchestratorError::Cancelled,
        other => {
            tracing::debug!(provider = provider_name, %other, "provider request failed");
            OrchestratorError::Provider(other)
        }
    })
}

pub(crate) async fn collect_stream_with_timeouts(
    mut stream: CompletionStream,
    cancel: &CancellationToken,
    first_byte_timeout: Duration,
    idle_timeout: Duration,
) -> Result<(String, Option<String>, Vec<ToolCall>, Option<CompletionUsage>), ProviderError> {
    use futures::StreamExt;

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = None;
    let mut first_chunk = true;
    loop {
        let timeout = if first_chunk { first_byte_timeout } else { idle_timeout };
        let phase = if first_chunk { "time-to-first-byte" } else { "stream-idle" };
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = tokio::time::timeout(timeout, stream.next()) => {
                result.map_err(|_| ProviderError::Timeout {
                    phase,
                    timeout,
                })?
            }
        };
        let Some(chunk) = next else {
            break;
        };
        first_chunk = false;
        let chunk = chunk?;
        text.push_str(&chunk.delta);
        if let Some(r) = chunk.reasoning {
            reasoning.push_str(&r);
        }
        if let Some(tool_call) = chunk.tool_call {
            tool_calls.push(tool_call);
        }
        if chunk.is_final {
            usage = chunk.usage;
        }
    }
    let reasoning = if reasoning.is_empty() { None } else { Some(reasoning) };
    Ok((text, reasoning, tool_calls, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[test]
    fn build_replaces_placeholders() {
        let builder = PromptBuilder::new(
            "System prompt\nWM: {working_memory}\nSummary: {summary}".to_string(),
        );

        let request = builder.build("<memory>test</memory>", &[], Some("Previous summary"), None);

        assert_eq!(request.messages.len(), 1);
        let system_msg = &request.messages[0];
        assert_eq!(system_msg.role, Role::System);
        assert!(system_msg.content.contains("<memory>test</memory>"));
        assert!(system_msg.content.contains("Previous summary"));
    }

    #[test]
    fn messages_appended_after_system() {
        let builder = PromptBuilder::new("System prompt".to_string());

        let user_msg = Message {
            role: Role::User,
            content: "Hello".to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };

        let request = builder.build("", &[user_msg], None, None);

        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(request.messages[1].role, Role::User);
    }

    #[test]
    fn skills_section_appended_when_present() {
        use crate::skills_context::SkillsContext;
        use concerto_skills::SkillManager;
        use std::fs;
        use std::io::Write as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let pack_dir = temp.path().join("rust-testing");
        fs::create_dir_all(&pack_dir).expect("create pack dir");
        let mut manifest = fs::File::create(pack_dir.join("skill.toml")).expect("create manifest");
        manifest
            .write_all(
                b"id = \"rust-testing\"\nname = \"Rust Testing\"\nversion = \"1.0.0\"\ndescription = \"t\"\ninstructions = \"Write tests first.\"\n",
            )
            .expect("write manifest");

        let skills = Arc::new(SkillsContext::new(
            Arc::new(SkillManager::new(vec![temp.path().to_path_buf()])),
            None,
            true,
            4000,
        ));
        skills.refresh().expect("refresh succeeds");

        let builder = PromptBuilder::with_skills("System prompt".to_string(), Some(skills.clone()));
        let request = builder.build("", &[], None, None);
        let system = &request.messages[0].content;
        assert!(system.contains("## Skills"), "skills section missing: {system}");
        assert!(system.contains("Write tests first."));

        // A plain `new` builder never gains a skills section.
        let plain = PromptBuilder::new("System prompt".to_string());
        let request = plain.build("", &[], None, None);
        assert!(!request.messages[0].content.contains("## Skills"));

        // Empty context section is not appended either.
        let empty = SkillsContext::default();
        let builder =
            PromptBuilder::with_skills("System prompt".to_string(), Some(Arc::new(empty)));
        let request = builder.build("", &[], None, None);
        let system = &request.messages[0].content;
        assert!(system.starts_with("System prompt"), "content: {system}");
        assert!(!system.contains("## Skills"), "unexpected skills section: {system}");
        // The identity card is always appended (OS fallback when no profile
        // is set), so exact-equality assertions on the built system prompt
        // are not possible anymore.
        assert!(system.contains("## Environment"), "identity card missing: {system}");
    }

    // -------------------------------------------------------------------
    // OS/shell identity card
    // -------------------------------------------------------------------

    fn fake_profile(executable: &str, backend: ShellBackendType) -> ShellProfileConfig {
        ShellProfileConfig {
            id: "test-profile".to_string(),
            name: "Test Profile".to_string(),
            backend,
            executable: executable.to_string(),
            ..ShellProfileConfig::default()
        }
    }

    #[test]
    fn environment_card_renders_bash_profile_with_posix_notes() {
        let card = environment_card(Some(&fake_profile("/usr/bin/bash", ShellBackendType::System)));
        assert!(card.contains("## Environment"), "card: {card}");
        assert!(
            card.contains(&format!("- OS: {} ({})", std::env::consts::OS, std::env::consts::ARCH)),
            "OS/arch line missing: {card}"
        );
        assert!(
            card.contains("profile `test-profile` — bash (backend: system)"),
            "shell line missing: {card}"
        );
        assert!(card.contains("(posix)"), "posix notes header missing: {card}");
        assert!(card.contains("Quote every `$VAR`"), "posix gotcha missing: {card}");
        assert!(!card.contains("powershell"), "wrong dialect notes present: {card}");
        assert!(!card.contains("$LASTEXITCODE"), "powershell gotcha present: {card}");
    }

    // ── per-OS gotchas (smoke follow-up round 2: no /tmp on Windows) ──────

    /// POSIX hosts keep the card as-is: no OS-specific gotcha line is added.
    #[test]
    fn platform_notes_posix_adds_nothing() {
        for os in ["linux", "macos", "freebsd", "openbsd"] {
            assert!(
                platform_notes(os).is_empty(),
                "POSIX platform '{os}' must contribute no extra card line"
            );
        }
    }

    /// Windows hosts get the no-`/tmp` gotcha: scratch files belong in
    /// `%TEMP%` or the project directory (the smoke session tried
    /// `/tmp/example.bin` on Windows and was correctly contained — the card
    /// must tell the model before the policy has to).
    #[test]
    fn platform_notes_windows_warn_about_tmp() {
        let notes = platform_notes("windows");
        assert_eq!(notes.len(), 1, "exactly one Windows gotcha line");
        let note = &notes[0];
        for needle in ["`/tmp`", "%TEMP%", "project directory"] {
            assert!(note.contains(needle), "Windows gotcha missing '{needle}': {note}");
        }
    }

    /// The rendered card delegates the note placement to `platform_notes` —
    /// exercised here for a POSIX host (the note set for the compile target).
    #[test]
    fn environment_card_processes_platform_notes() {
        let card = environment_card(Some(&fake_profile("/usr/bin/bash", ShellBackendType::System)));
        let notes = platform_notes(std::env::consts::OS);
        for note in notes {
            assert!(card.contains(&note), "platform note missing in card: {card}");
        }
        // Exactly the expected notes appear; no Windows line on POSIX hosts.
        if std::env::consts::OS != "windows" {
            assert!(
                !card.contains("%TEMP%"),
                "no Windows gotcha may leak onto POSIX cards: {card}"
            );
        }
    }

    #[test]
    fn environment_card_renders_powershell_profile_with_powershell_notes() {
        let card = environment_card(Some(&fake_profile("pwsh.exe", ShellBackendType::Custom)));
        assert!(card.contains("(powershell)"), "powershell notes header missing: {card}");
        assert!(card.contains("no `&&`"), "powershell gotcha missing: {card}");
        assert!(card.contains("$LASTEXITCODE"), "powershell gotcha missing: {card}");
        assert!(!card.contains("(posix)"), "posix notes present: {card}");
        assert!(!card.contains("`echo -e`"), "posix gotcha present: {card}");
        assert!(card.contains("backend: custom"), "backend label missing: {card}");
    }

    #[test]
    fn environment_card_unknown_executable_is_generic() {
        let card =
            environment_card(Some(&fake_profile("/opt/tools/fruitloop", ShellBackendType::System)));
        assert!(card.contains("(generic)"), "generic notes header missing: {card}");
        assert!(card.contains("Use the simplest single command"), "generic gotcha missing: {card}");
        assert!(!card.contains("Quote every `$VAR`"), "posix notes present: {card}");
        // The unknown executable is still reported verbatim in the shell line.
        assert!(
            card.contains("— fruitloop (backend: system)"),
            "unknown executable label missing: {card}"
        );
    }

    #[test]
    fn environment_card_without_profile_falls_back_to_os_facts() {
        let card = environment_card(None);
        assert!(card.contains("## Environment"), "card: {card}");
        assert!(
            card.contains(&format!("- OS: {} ({})", std::env::consts::OS, std::env::consts::ARCH)),
            "OS/arch line missing: {card}"
        );
        assert!(
            card.contains("Agent shell: OS default (`"),
            "OS-default shell line missing: {card}"
        );
        // Never empty, never panics, and always carries dialect notes.
        assert!(card.contains("Shell dialect notes"), "dialect notes missing: {card}");
    }

    #[test]
    fn build_appends_identity_card_when_profile_set() {
        let profile = fake_profile("/bin/bash", ShellBackendType::System);
        let builder =
            PromptBuilder::new("System prompt".to_string()).with_shell_profile(Some(profile));
        let request = builder.build("", &[], None, None);
        let system = &request.messages[0].content;
        assert!(system.starts_with("System prompt"), "content: {system}");
        assert!(system.contains("profile `test-profile` — bash (backend: system)"), "{system}");
        assert!(system.contains("(posix)"), "dialect notes missing: {system}");
    }

    #[test]
    fn build_without_profile_appends_os_fallback_card() {
        let builder = PromptBuilder::new("System prompt".to_string());
        let request = builder.build("", &[], None, None);
        let system = &request.messages[0].content;
        assert!(system.starts_with("System prompt"), "content: {system}");
        assert!(system.contains("## Environment"), "identity card missing: {system}");
        assert!(
            !system.contains("profile `"),
            "no profile facts may appear without a profile: {system}"
        );
    }

    #[test]
    fn parses_fenced_json() {
        let value = parse_json_value("```json\n{\"ok\":true}\n```").unwrap();
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn parses_json_surrounded_by_prose() {
        let value = parse_json_value("Here is the result: {\"goals\":[\"ship\"]} Thanks.").unwrap();
        assert_eq!(value["goals"][0], "ship");
    }

    #[test]
    fn ignores_braces_inside_json_strings() {
        let value =
            parse_json_value("prefix {\"text\":\"literal } and { braces\",\"ok\":true} suffix")
                .unwrap();
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn skips_invalid_candidate_and_uses_later_valid_json() {
        let value = parse_json_value("not-json {broken} then {\"valid\":1}").unwrap();
        assert_eq!(value["valid"], 1);
    }

    #[test]
    fn invalid_text_returns_none() {
        assert!(parse_json_value("nothing structured here").is_none());
    }

    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct TestPlanItem {
        task: String,
    }

    #[test]
    fn substring_finds_plan_array_behind_thinking_prose() {
        // A plan array wrapped in "here's a thinking process" prose with a
        // trailing ```json fence. The prose contains no other balanced
        // fragments, so candidate iteration must reach the fenced array.
        let input = concat!(
            "Here's a thinking process:\n\n",
            "1. **Analyze User Input:** Understand what the user wants.\n",
            "2. **Produce JSON:** emit the plan array.\n\n",
            "```json\n",
            "[{\"task\": \"First\"}, {\"task\": \"Second\"}]\n",
            "```\n",
        );
        let parsed: Option<Vec<TestPlanItem>> = parse_json_substring(input);
        assert_eq!(
            parsed,
            Some(vec![
                TestPlanItem { task: "First".into() },
                TestPlanItem { task: "Second".into() }
            ])
        );
    }

    #[test]
    fn substring_normalizes_smart_quotes_and_non_breaking_hyphens() {
        // U+201C/U+201D delimit the JSON strings and U+2011 (non-breaking
        // hyphen) appears inside a value; both break strict serde_json, which
        // previously also confused the string tracking in the balanced-bracket
        // scan. Normalization maps them to ASCII before parsing.
        let input = concat!(
            "Result: {\u{201C}task\u{201D}: ",
            "\u{201C}write\u{2011}commit\u{2011}code\u{201D}} done",
        );
        let parsed: Option<TestPlanItem> = parse_json_substring(input);
        assert_eq!(parsed, Some(TestPlanItem { task: "write-commit-code".into() }));
    }

    #[test]
    fn substring_skips_earlier_object_that_does_not_deserialize_to_target() {
        // The prose starts with a balanced object that is valid JSON but not a
        // `Vec<TestPlanItem>`; candidate iteration must keep going and return
        // the real plan array instead of returning None.
        let input =
            "Summary: {\"note\": \"not the plan\"} then the real plan: [{\"task\": \"Ship\"}]";
        let parsed: Option<Vec<TestPlanItem>> = parse_json_substring(input);
        assert_eq!(parsed, Some(vec![TestPlanItem { task: "Ship".into() }]));
    }

    #[test]
    fn substring_strict_and_fenced_json_still_parse() {
        assert_eq!(
            parse_json_substring::<Vec<String>>("[\"one\", \"two\"]"),
            Some(vec!["one".to_string(), "two".to_string()])
        );
        assert_eq!(
            parse_json_substring::<Vec<String>>("```json\n[\"one\"]\n```"),
            Some(vec!["one".to_string()])
        );
    }

    #[tokio::test]
    async fn stream_without_first_chunk_times_out() {
        let pending: CompletionStream = Box::pin(stream::pending());
        let result = collect_stream_with_timeouts(
            pending,
            &CancellationToken::new(),
            Duration::from_millis(1),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(ProviderError::Timeout { phase: "time-to-first-byte", .. })));
    }

    #[tokio::test]
    async fn collect_stream_threads_reasoning_through() {
        use concerto_core::types::CompletionChunk;
        let stream: CompletionStream = Box::pin(stream::iter(vec![
            Ok(CompletionChunk {
                delta: "part one".into(),
                reasoning: Some("reason one".into()),
                tool_call: None,
                is_final: false,
                usage: None,
            }),
            Ok(CompletionChunk {
                delta: "".into(),
                reasoning: Some("reason two".into()),
                tool_call: None,
                is_final: false,
                usage: None,
            }),
            Ok(CompletionChunk {
                delta: " part two".into(),
                reasoning: None,
                tool_call: None,
                is_final: true,
                usage: None,
            }),
        ]));
        let (text, reasoning, tool_calls, usage) = collect_stream(stream).await.unwrap();
        assert_eq!(text, "part one part two");
        assert_eq!(reasoning.as_deref(), Some("reason onereason two"));
        assert!(tool_calls.is_empty());
        // No chunk carries a usage report, so `usage` stays `None`.
        assert_eq!(usage, None);
    }

    #[tokio::test]
    async fn collect_stream_reasoning_none_when_absent() {
        use concerto_core::types::CompletionChunk;
        let stream: CompletionStream = Box::pin(stream::iter(vec![Ok(CompletionChunk {
            delta: "no reasoning here".into(),
            reasoning: None,
            tool_call: None,
            is_final: true,
            usage: None,
        })]));
        let (text, reasoning, tool_calls, usage) = collect_stream(stream).await.unwrap();
        assert_eq!(text, "no reasoning here");
        assert_eq!(reasoning, None);
        assert!(tool_calls.is_empty());
        assert_eq!(usage, None);
    }

    #[tokio::test]
    async fn collect_stream_captures_usage_from_final_chunk() {
        use concerto_core::types::{CompletionChunk, CompletionUsage};
        let stream: CompletionStream = Box::pin(stream::iter(vec![
            Ok(CompletionChunk {
                delta: "hi".into(),
                reasoning: None,
                tool_call: None,
                is_final: false,
                usage: None,
            }),
            Ok(CompletionChunk {
                delta: "".into(),
                reasoning: None,
                tool_call: None,
                is_final: true,
                usage: Some(CompletionUsage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                }),
            }),
        ]));
        let (_, _, _, usage) = collect_stream(stream).await.unwrap();
        assert_eq!(
            usage,
            Some(CompletionUsage { prompt_tokens: Some(10), completion_tokens: Some(5) })
        );
    }

    // -- ADR-048: cache_stable_prefix --------------------------------------

    fn user_message(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    #[test]
    fn cache_stable_prefix_defaults_to_false() {
        assert!(!PromptBuilder::new("System prompt").cache_stable_prefix());
        assert!(!PromptBuilder::with_skills("System prompt", None).cache_stable_prefix());
        assert!(PromptBuilder::new("System prompt")
            .with_cache_stable_prefix(true)
            .cache_stable_prefix());
    }

    /// Prefix stability: two builds over different working memory and a
    /// different conversation must emit byte-identical head bytes.
    #[test]
    fn stable_head_is_byte_identical_across_builds_with_different_tails() {
        let builder = PromptBuilder::new(
            "System prompt\nWM: {working_memory}\nSummary: {summary}".to_string(),
        )
        .with_cache_stable_prefix(true);
        let head = builder.stable_system_head(Some("session summary"));

        let first = builder.build(
            "<working_memory>iteration 1</working_memory>",
            &[user_message("turn one")],
            Some("session summary"),
            None,
        );
        let second = builder.build(
            "<working_memory>iteration 2: more files, different retrieved chunks</working_memory>",
            &[user_message("turn two")],
            Some("session summary"),
            None,
        );

        let first_system = &first.messages[0].content;
        let second_system = &second.messages[0].content;
        assert!(first_system.starts_with(&head), "the head must lead the system message");
        assert!(second_system.starts_with(&head), "head bytes must be stable across builds");
        assert_ne!(first_system, second_system, "the volatile tail must still differ");
        assert_ne!(
            first.messages[1].content, second.messages[1].content,
            "the conversation sits outside the head"
        );
    }

    /// Volatile-tail exclusion: neither the working memory (active state plus
    /// retrieved chunks) nor the conversation may appear inside the pinned
    /// head.
    #[test]
    fn cache_stable_prefix_keeps_volatile_content_out_of_the_head() {
        let builder = PromptBuilder::new("System prompt\nWM: {working_memory}".to_string())
            .with_cache_stable_prefix(true);
        let head = builder.stable_system_head(None);
        let request = builder.build(
            "<working_memory>RETRIEVED_CHUNK_SECRET</working_memory>",
            &[user_message("USER_TURN_SECRET")],
            None,
            None,
        );

        let system = &request.messages[0].content;
        assert!(system.starts_with(&head));
        assert!(
            !head.contains("RETRIEVED_CHUNK_SECRET"),
            "working memory must not sit in the head"
        );

        let marker = system.find("RETRIEVED_CHUNK_SECRET").expect("the tail is still delivered");
        assert!(
            marker >= head.len(),
            "the tail must start after the head: marker {marker}, head {}",
            head.len()
        );
        assert!(
            !system.contains("USER_TURN_SECRET"),
            "the conversation never enters the system message"
        );
        assert_eq!(request.messages[1].content, "USER_TURN_SECRET");
    }

    /// Production-shaped templates carry no `{working_memory}` placeholder, so
    /// the tail is appended after every stable section (skills, project
    /// context, environment card) rather than interleaved into them.
    #[test]
    fn cache_stable_prefix_appends_the_tail_after_the_stable_sections() {
        let builder =
            PromptBuilder::new("System prompt".to_string()).with_cache_stable_prefix(true);
        let request = builder.build("<working_memory>state</working_memory>", &[], None, None);
        let system = &request.messages[0].content;

        let card = system.find("## Environment").expect("environment card present");
        let tail = system.find("<working_memory>state</working_memory>").expect("tail present");
        assert!(tail > card, "tail at {tail} must follow the environment card at {card}");
    }

    /// The default path is untouched: the working memory is substituted where
    /// the template asks for it — once, before the stable sections — so
    /// `cache_stable_prefix = false` stays byte-identical to today.
    #[test]
    fn cache_stable_prefix_off_keeps_todays_assembly() {
        let template = "System prompt\nWM: {working_memory}\nSummary: {summary}";
        let builder = PromptBuilder::new(template.to_string());
        assert!(!builder.cache_stable_prefix(), "the knob must default to off");

        let request = builder.build("<working_memory>state</working_memory>", &[], None, None);
        let system = &request.messages[0].content;
        assert!(
            system.starts_with(
                "System prompt\nWM: <working_memory>state</working_memory>\nSummary: "
            ),
            "in-place substitution must be byte-identical to today: {system}"
        );
        assert_eq!(
            system.matches("<working_memory>state</working_memory>").count(),
            1,
            "the working memory must not be duplicated"
        );
        let card = system.find("## Environment").expect("environment card present");
        let wm =
            system.find("<working_memory>state</working_memory>").expect("working memory present");
        assert!(wm < card, "today's order keeps the working memory inside the head");
    }

    /// Tool schemas ride `CompletionRequest::tools`, not the system string:
    /// two builds over the same registry serialize to identical bytes while
    /// the message tail churns.
    #[test]
    fn tool_schemas_stay_byte_stable_across_builds() {
        use concerto_core::types::ToolDefinition;

        let tools = [ToolDefinition {
            name: "filesystem".into(),
            description: "Filesystem operations".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let builder =
            PromptBuilder::new("System prompt".to_string()).with_cache_stable_prefix(true);

        let first = builder.build("<wm>one</wm>", &[user_message("one")], None, Some(&tools));
        let second = builder.build("<wm>two</wm>", &[user_message("two")], None, Some(&tools));

        assert_eq!(
            serde_json::to_string(&first.tools).expect("tools serialize"),
            serde_json::to_string(&second.tools).expect("tools serialize"),
            "tool schemas must not depend on the volatile tail"
        );
        assert_ne!(first.messages[0].content, second.messages[0].content);
    }

    // -- default-path working-memory delivery (shipped SYSTEM_PROMPT_* -------

    const WORKING_MEMORY_BLOCK: &str =
        "<working_memory>\n{\"objective\":\"ship the fix\"}\n</working_memory>";

    /// The shipped build template carries `{working_memory}`, so the
    /// active-state + retrieved-chunks block is delivered on the DEFAULT
    /// path (`cache_stable_prefix` off) — inside the single system message,
    /// which is the one last-system-wins adapters (Anthropic, Gemini) read.
    #[test]
    fn working_memory_block_reaches_the_model_on_the_default_path() {
        let builder = PromptBuilder::new(concerto_core::types::SYSTEM_PROMPT_BUILD.to_string());
        assert!(!builder.cache_stable_prefix(), "this must exercise the default path");

        let request = builder.build(WORKING_MEMORY_BLOCK, &[user_message("do it")], None, None);

        let system: Vec<_> =
            request.messages.iter().filter(|message| message.role == Role::System).collect();
        assert_eq!(system.len(), 1, "exactly one system message, kept intact");
        assert!(
            system[0].content.contains(WORKING_MEMORY_BLOCK),
            "working-memory block missing from the system message"
        );
        assert!(system[0].content.contains("## Environment"), "structure intact");
    }

    /// An empty block degrades to an empty string: no placeholder leak, no
    /// dangling blank-line separator, no fabricated section header — the
    /// prompt is the bare template plus the usual appended sections.
    #[test]
    fn empty_working_memory_degrades_to_an_empty_string() {
        let builder = PromptBuilder::new(concerto_core::types::SYSTEM_PROMPT_BUILD.to_string());
        let request = builder.build("", &[user_message("do it")], None, None);
        let system = &request.messages[0].content;

        assert!(!system.contains("{working_memory}"), "placeholder leaked: {system}");
        assert!(
            !system.contains("<working_memory>"),
            "an empty block must not render a section: {system}"
        );
        assert!(!system.contains("\n\n\n"), "the separator must go with the empty block: {system}");
        assert!(system.contains("## Environment"), "environment card intact: {system}");
        assert_eq!(
            request.messages.iter().filter(|message| message.role == Role::System).count(),
            1,
            "single-system-message structure preserved"
        );
    }

    /// The volatile tail still lands after the environment card when the
    /// ADR-048 knob is on: the placeholder is blanked inside the stable head
    /// and the block is appended, never injected twice.
    #[test]
    fn cache_stable_prefix_delivers_the_block_once_after_the_head() {
        let builder = PromptBuilder::new(concerto_core::types::SYSTEM_PROMPT_BUILD.to_string())
            .with_cache_stable_prefix(true);
        let request = builder.build(WORKING_MEMORY_BLOCK, &[], None, None);
        let system = &request.messages[0].content;
        let head = builder.stable_system_head(None);

        assert!(system.starts_with(&head), "the stable head still leads");
        assert!(!head.contains(WORKING_MEMORY_BLOCK), "the block stays out of the head");
        assert_eq!(
            system.matches(WORKING_MEMORY_BLOCK).count(),
            1,
            "delivered exactly once: {system}"
        );
        let card = system.find("## Environment").expect("environment card present");
        let block = system.find(WORKING_MEMORY_BLOCK).expect("working memory present");
        assert!(block > card, "the volatile tail follows the stable sections");
    }
}
