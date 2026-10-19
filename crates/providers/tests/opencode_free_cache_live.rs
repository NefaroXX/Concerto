//! Live prompt-cache checks for the anonymous OpenCode Zen free models.
//!
//! These tests hit the real relay and are therefore `#[ignore]`d and gated by
//! `CONCERTO_LIVE_OPENCODE=1`. They cost nothing on the free tier, but they
//! still share an IP-keyed daily bucket. A transient `429` is retried; one
//! that persists fails the test with the captured error. If the bucket is
//! exhausted, record that as an environment observation rather than a code
//! regression.
//!
//! Run with:
//!
//! ```text
//! CONCERTO_LIVE_OPENCODE=1 cargo test -p concerto-providers \
//!     --test opencode_free_cache_live -- --ignored --nocapture
//! ```
//!
//! The property under test is the relay's implicit prefix cache: request 2
//! repeats request 1's message sequence verbatim and appends two turns, so the
//! provider should report the request-1 prompt as `cached_tokens`. The stable
//! system prompt is deliberately large (well above 8k tokens) because GLM only
//! starts reporting cache reads once the reuseable prefix is large enough.

use std::time::Duration;

use concerto_core::error::ProviderError;
use concerto_core::traits::provider::{CompletionStream, LlmProvider};
use concerto_core::types::{
    CompletionChunk, CompletionRequest, CompletionUsage, Message, Role, ToolChoice,
};
use concerto_core::CancellationToken;
use concerto_providers::opencode::OpenCodeZenProvider;
use futures::StreamExt;

const DEEPSEEK: &str = "deepseek-v4.1-flash-free";
const GLM: &str = "glm-5.3-flash-free";

/// Time allowed for a provider to make the first prompt's prefix reusable.
const CACHE_SETTLE: Duration = Duration::from_secs(10);

/// A repeated continuation warms/hits different relay replicas; a short gap
/// is enough because the prefix is already cached somewhere upstream.
const RETRY_SETTLE: Duration = Duration::from_secs(2);

/// DeepSeek's cache is content-addressed and hits on the first continuation.
/// GLM's cache is also sensitive to which relay replica serves the request, so
/// its probe retries the identical continuation a bounded number of times and
/// still requires one real >=50% hit.
const DEEPSEEK_ATTEMPTS: usize = 1;
const GLM_ATTEMPTS: usize = 6;

/// `#[ignore]`d tests still execute when `--ignored` is passed, so the runtime
/// env gate is the second half of the contract: without it every live test
/// returns immediately with a visible skip line.
fn live_enabled() -> bool {
    match std::env::var("CONCERTO_LIVE_OPENCODE") {
        Ok(value) if value.trim() == "1" => true,
        _ => {
            eprintln!(
                "SKIP live OpenCode cache probe: set CONCERTO_LIVE_OPENCODE=1 to hit the free tier"
            );
            false
        }
    }
}

#[tokio::test]
#[ignore = "live network: set CONCERTO_LIVE_OPENCODE=1, run with --ignored"]
async fn deepseek_free_model_reuses_a_stable_prompt_prefix() {
    if !live_enabled() {
        return;
    }
    probe_cache(DEEPSEEK, DEEPSEEK_ATTEMPTS).await;
}

#[tokio::test]
#[ignore = "live network: set CONCERTO_LIVE_OPENCODE=1, run with --ignored"]
async fn glm_free_model_reuses_a_stable_prompt_prefix() {
    if !live_enabled() {
        return;
    }
    probe_cache(GLM, GLM_ATTEMPTS).await;
}

/// Send the two-request probe and assert the second request reused at least
/// half of its prompt from cache.
async fn probe_cache(model: &str, cache_attempts: usize) {
    let provider = OpenCodeZenProvider::new(String::new(), model.to_string(), 120);
    // One caller session for the whole probe: the provider mirrors the real
    // client's stable `ses_` wire id across the two completions, which is what
    // the relay uses for backend/cache affinity.
    let session = concerto_core::ids::new_id();
    let system = stable_system_prompt();
    eprintln!(
        "live cache probe model={model} system_bytes={} approx_tokens~{}",
        system.len(),
        system.len() / 4
    );

    let first_messages = vec![
        text_message(Role::System, system.clone()),
        text_message(Role::User, "Reply with exactly CACHE_PROBE_ONE. Do not call any tools."),
    ];
    let (first_text, first_usage) =
        run_once(&provider, request_with(first_messages, session)).await;
    eprintln!(
        "model={model} request=1 prompt_tokens={:?} cached_tokens={:?} completion_tokens={:?}",
        first_usage.prompt_tokens, first_usage.cache_read_tokens, first_usage.completion_tokens
    );

    // DeepSeek's context cache is disk-backed and becomes visible a few
    // seconds after the first request; without this pause request 2 can race
    // the cache write and report zero even though the prefix is identical.
    tokio::time::sleep(CACHE_SETTLE).await;

    let second_messages = vec![
        text_message(Role::System, system),
        text_message(Role::User, "Reply with exactly CACHE_PROBE_ONE. Do not call any tools."),
        text_message(Role::Assistant, first_text.trim().to_string()),
        text_message(
            Role::User,
            "Continue the same task and reply with exactly CACHE_PROBE_TWO. Do not call any tools.",
        ),
    ];

    // DeepSeek asserts on this exact second request. GLM's cache hit depends
    // on the replica chosen by the shared relay, so its probe repeats the
    // identical continuation a bounded number of times; the assertion still
    // requires a real >=50% hit on one of them.
    let mut best: Option<CompletionUsage> = None;
    for attempt in 1..=cache_attempts {
        let (_, usage) = run_once(&provider, request_with(second_messages.clone(), session)).await;
        eprintln!(
            "model={model} continuation={attempt}/{cache_attempts} prompt_tokens={:?} \
             cached_tokens={:?} completion_tokens={:?}",
            usage.prompt_tokens, usage.cache_read_tokens, usage.completion_tokens
        );
        if qualifies_as_hit(&usage) {
            best = Some(usage);
            break;
        }
        if best.as_ref().is_none_or(|previous| usage.cache_read_tokens > previous.cache_read_tokens)
        {
            best = Some(usage);
        }
        if attempt < cache_attempts {
            tokio::time::sleep(RETRY_SETTLE).await;
        }
    }

    let second_usage = best.expect("at least one continuation attempt ran");
    let prompt_tokens = second_usage.prompt_tokens.expect("request 2 must report prompt_tokens");
    let cached_tokens =
        second_usage.cache_read_tokens.expect("request 2 must report cached_tokens");
    assert!(
        cached_tokens > 0,
        "model {model}: no continuation reported cache reads after {cache_attempts} attempts \
         (best prompt_tokens={prompt_tokens})"
    );
    assert!(
        cached_tokens * 2 >= prompt_tokens,
        "model {model}: cache hit rate below 50% (cached={cached_tokens}, prompt={prompt_tokens})"
    );
}

/// Whether one continuation reused at least half of its prompt from cache.
fn qualifies_as_hit(usage: &CompletionUsage) -> bool {
    match (usage.prompt_tokens, usage.cache_read_tokens) {
        (Some(prompt), Some(cached)) => cached > 0 && cached * 2 >= prompt,
        _ => false,
    }
}

/// One completion with up to three total attempts on the relay's rate limit.
/// `RateLimit` is an ordinary 429 (the anonymous tier shares one IP-keyed
/// daily bucket), so half the advertised `Retry-After` (capped at 30s) is a
/// reasonable backoff; a persistent limit surfaces as the original error.
async fn run_once(
    provider: &OpenCodeZenProvider,
    request: CompletionRequest,
) -> (String, CompletionUsage) {
    for attempt in 1..=3 {
        match provider.stream_completion(request.clone(), CancellationToken::new()).await {
            Ok(stream) => {
                let (text, usage) = drain(stream).await;
                return (text, usage.expect("terminal chunk must report usage"));
            }
            Err(ProviderError::RateLimit { retry_after }) if attempt < 3 => {
                let seconds = (retry_after.as_secs_f64() / 2.0).min(30.0);
                eprintln!(
                    "rate limited on attempt {attempt}/3, retrying after {seconds:.1}s \
                     (retry_after={retry_after:?})"
                );
                tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            }
            Err(error) => panic!("provider call failed on attempt {attempt}/3: {error}"),
        }
    }
    unreachable!("the retry loop returns on every path")
}

/// Drain one stream, preserving the assistant text and the terminal usage.
async fn drain(mut stream: CompletionStream) -> (String, Option<CompletionUsage>) {
    let mut text = String::new();
    let mut usage = None;
    while let Some(chunk) = stream.next().await {
        let CompletionChunk { delta, usage: chunk_usage, .. } =
            chunk.expect("stream chunk must not fail");
        text.push_str(&delta);
        if chunk_usage.is_some() {
            usage = chunk_usage;
        }
    }
    (text, usage)
}

/// A ~60k-character (roughly 15k-token) block of repeated convention lines.
/// Comfortably above GLM's observed ~6k-token cache floor and DeepSeek's
/// much lower one, while staying far inside both models' context windows.
///
/// A per-process salt makes every run start from a cold prefix, so a pass
/// proves this run's first request warmed the cache rather than a previous
/// run having done so.
fn stable_system_prompt() -> String {
    let salt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let mut prompt = format!(
        "You are a careful coding assistant. Follow these project conventions exactly. \
         Probe run: {salt}-{}.\n",
        std::process::id()
    );
    for line in 0..600 {
        prompt.push_str(&format!(
            "{line:04}: Prefer the smallest sound change; verify behavior before claiming success; \
             keep invariants documented; never rewrite unrelated code.\n"
        ));
    }
    prompt
}

fn request_with(messages: Vec<Message>, session: concerto_core::ids::Ulid) -> CompletionRequest {
    CompletionRequest {
        session_id: Some(session),
        stable_system_prefix_bytes: None,
        model: String::new(),
        messages,
        tools: None,
        tool_choice: Some(ToolChoice::None),
        temperature: Some(0.0),
        max_tokens: Some(64),
        stream: true,
    }
}

fn text_message(role: Role, content: impl Into<String>) -> Message {
    Message {
        role,
        content: content.into(),
        tool_calls: None,
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
    }
}
