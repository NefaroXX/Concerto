//! Prompt builder — assembles `CompletionRequest` instances from task,
//! working memory, conversation history, and session summary.
//!
//! Every completion call in the orchestrator goes through this builder,
//! ensuring consistent injection of the working memory block, system
//! prompt, and previous session summary.

use std::sync::Arc;
use std::time::Duration;

use concerto_core::error::ProviderError;
use concerto_core::event::EventBus;
use concerto_core::ids::Ulid;
use concerto_core::text::normalize_typographic;
use concerto_core::traits::provider::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, CompletionUsage, Message, Role, ToolCall};
use concerto_core::{CancellationToken, OrchestratorError, TaskId};
use concerto_providers::retry::{with_provider_retry, RetryPolicy};

use crate::skills_context::SkillsContext;

/// Builds the full `CompletionRequest` for each agent cycle.
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    /// The system prompt template. `{working_memory}` and `{summary}`
    /// placeholders are replaced at build time.
    system_template: String,
    /// Runtime-owned skills context (ADR-43, Task 4). When set and non-empty,
    /// the current skills section is appended to the system prompt on every
    /// build, so a live refresh takes effect without rebuilding the builder.
    skills: Option<Arc<SkillsContext>>,
}

impl PromptBuilder {
    /// Create a new builder with the given system prompt template and no
    /// skills context.
    pub fn new(system_template: impl Into<String>) -> Self {
        Self { system_template: system_template.into(), skills: None }
    }

    /// Create a builder that appends the enabled skills section to the system
    /// prompt. Pass `None` to keep the plain `new` behavior.
    pub fn with_skills(
        system_template: impl Into<String>,
        skills: Option<Arc<SkillsContext>>,
    ) -> Self {
        Self { system_template: system_template.into(), skills }
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
        let mut system = self.system_template.clone();

        if let Some(summary) = prev_summary {
            system = system.replace("{summary}", summary);
        } else {
            system = system.replace("{summary}", "");
        }

        system = system.replace("{working_memory}", working_memory_block);

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
        assert_eq!(request.messages[0].content, "System prompt");
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
}
