//! Phase 0 intent routing: deterministic, pure, negation-aware classification
//! of a raw user request into a requested outcome and a task scope.
//!
//! ADR-55 Phase 0: this module defines the routing vocabulary and a pure
//! [`route()`] function. Phase 0 ships the router as the ADR-55 intent gate's
//! classifier: the gate wiring in `crates/orchestrator/src/runtime_runner.rs`
//! routes the user request and consumes [`RouterOutput`], and
//! `crates/orchestrator/src/intent_grants.rs` turns the user's confirmation
//! into run-scoped grants. The LLM intent classifier (ADR-55 Phase 2c;
//! ADR-56) lives in `crates/orchestrator/src/intent_classifier.rs` as a
//! wrapper around [`route()`]: when enabled it is the **primary** intent
//! decider, while these deterministic rules remain the offline / fail-soft
//! fallback. [`RouterRoute::LlmClassifier`] is never produced by this module
//! itself — only the orchestrator's wrapper emits it. Keeping the router pure
//! and free of any I/O makes it trivially testable and safe to slot into any
//! front-end later.
//!
//! # Keyword matching (word boundaries)
//!
//! The explicit outcome keywords (`Execute` / `diagnose` / `verify` / `plan` /
//! `review`) fire only on **word boundaries**: the normalized input is split
//! on whitespace with surrounding punctuation stripped, and a keyword matches
//! only when it equals a whole token (or, for phrases such as `run the tests`,
//! a run of consecutive tokens). Internal punctuation — hyphens, underscores,
//! apostrophes, slashes — stays part of a token, so benign words no longer
//! false-route to Execute: `fixture` no longer matches `fix`, `recreate` no
//! longer matches `create`, `additionally` no longer matches `add`, and
//! `write-up` no longer matches `write`.
//!
//! Inflected forms the corpus intends are listed **verbatim**, not stemmed:
//! `writes`/`writing`, `fixes`/`fixing`, `creates`/`creating`, `refactor/
//! refactoring`, and so on. Other inflections (past tense `wrote`, `built`)
//! are deliberately excluded: they usually *describe* prior work rather than
//! request a change, and an audit trail should not treat them as action
//! requests. The negation corpus (routing step a, [`NEGATION_PHRASES`]) and
//! question detection (routing step b) keep their original standalone /
//! substring semantics — only the keyword matcher changed.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Confidence below which the router must not proceed with a rule- or
/// classifier-derived outcome.
///
/// Confidence is used ONLY for router-path selection (rule vs classifier vs
/// ask): low confidence ⇒ keep the run read-only and ask the user. This module
/// is deterministic — when no rule matches, the route MUST be
/// [`RouterRoute::AskUser`] with confidence `0.0`. The orchestrator's LLM
/// classifier (ADR-56) re-uses the same constant as its re-route threshold so
/// a classifier Execute re-route always clears the intent gate's arm-1
/// confirmation dialog.
pub const LOW_CONFIDENCE_THRESHOLD: f32 = 0.7;

/// The outcome the user requested for the current request.
///
/// Phase 0 derives this purely from the request text (see [`route()`]). The
/// variant list is deliberately the Phase 0 set; future phases may extend it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequestedOutcome {
    /// A text answer; no tool use implied.
    Answer,
    /// Investigate why something is broken and explain it.
    Diagnose,
    /// Read-only review/critique of existing code.
    Review,
    /// Produce a plan/design as text; no writes.
    Plan,
    /// Implement/change code (write path). A negation override drops this.
    Execute,
    /// Run tests/checks to verify a change.
    Verify,
}

/// User decision on a previously approved plan (ADR-55 Phase 1d).
///
/// The run loop asks this through
/// [`ApprovalSink::request_plan_approval`] when an action-required Execute
/// request matches a stored plan binding for the same objective. The
/// decision is kept in the intent vocabulary so the approval sink interface
/// (core) and the registry decision helper (orchestrator) share one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanDecision {
    /// Apply the previously approved plan now (audited authority; grants
    /// filesystem + git like a confirmed Execute).
    Apply,
    /// Discard the stored plan and plan this objective anew (read-only).
    Replan,
}

impl PlanDecision {
    /// Stable audit label for the decision (`apply` | `replan`). The
    /// `None`/dismissed case is rendered by the caller.
    pub fn name(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Replan => "replan",
        }
    }
}

/// Minimal Phase-0 task scope: the project a request targets and any file
/// paths it hints at.
///
/// Capability tiers (read-only vs write scopes), glob sets, and deeper scope
/// modelling are Phase 1; Phase 0 only carries the project root plus candidate
/// file paths extracted heuristically from the request text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TaskScope {
    /// Project root the request is scoped to.
    pub project_dir: PathBuf,
    /// Heuristically hinted files, normalized to absolute paths inside
    /// `project_dir`. May be empty when the request names no files.
    pub hinted_paths: Vec<PathBuf>,
}

/// Which routing path produced the final outcome.
///
/// The pure router is deterministic: [`route()`] returns
/// [`RouterRoute::RuleHit`] for every matched corpus and
/// [`RouterRoute::AskUser`] when nothing matched.
/// [`RouterRoute::LlmClassifier`] is produced only by the orchestrator's LLM
/// classifier wrapper (ADR-56), never by [`route()`] itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RouterRoute {
    /// A deterministic Phase-0 corpus matched; `rule` names the corpus that
    /// won (see the `RULE_*` constants).
    RuleHit { rule: &'static str },
    /// LLM classifier path — emitted only by the orchestrator's classifier
    /// wrapper re-routing a deterministic result above threshold (ADR-56);
    /// never by [`route()`].
    LlmClassifier,
    /// No rule matched — ask the user for clarification.
    AskUser,
}

/// Serialized rule-name constants referenced by [`RouterRoute::RuleHit`].
///
/// [`RouterRoute`] carries `rule` as a `&'static str`, which the serde derive
/// cannot deserialize, so `route()`-produced rule names are these constants and
/// serialization maps them back onto the same constants (see the manual serde
/// impl below). Kept private — the set is closed in Phase 0.
const RULE_NEGATION_OVERRIDE: &str = "negation_override";
const RULE_QUESTION: &str = "question";
const RULE_VERIFY: &str = "verify_keyword";
const RULE_PLAN: &str = "plan_keyword";
const RULE_REVIEW: &str = "review_keyword";
const RULE_DIAGNOSE: &str = "diagnose_keyword";
const RULE_EXECUTE: &str = "execute_keyword";
const RULE_SMALLTALK: &str = "smalltalk";

/// The full routing result for one request string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouterOutput {
    /// The outcome the router settled on.
    pub outcome: RequestedOutcome,
    /// Project root + hinted file paths extracted from the request.
    pub scope: TaskScope,
    /// Confidence in the outcome, consumed only for router-path selection
    /// (see [`LOW_CONFIDENCE_THRESHOLD`]).
    pub confidence: f32,
    /// Which routing path produced this output.
    pub route: RouterRoute,
}

/// Lifecycle stage of an agent run, as delineated by the intent router.
///
/// `Display` is the bare enum name in Phase 0; chip/label presentation is a UI
/// concern that lands in a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunStage {
    /// Grounding the request in the conversation/project context.
    Understand,
    /// Reading the relevant files and gathering evidence.
    Inspect,
    /// Designing the change before touching anything.
    Plan,
    /// Making the change.
    Execute,
    /// Running tests/checks against the change.
    Verify,
    /// The run is finished.
    Complete,
}

impl fmt::Display for RunStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Understand => "Understand",
            Self::Inspect => "Inspect",
            Self::Plan => "Plan",
            Self::Execute => "Execute",
            Self::Verify => "Verify",
            Self::Complete => "Complete",
        };
        f.write_str(label)
    }
}

/// Route `input` to a requested outcome with a deterministic, pure,
/// negation-aware rule set.
///
/// Order of evaluation (first match wins):
/// 1. **Negation corpus** — read-only directives (`don't`, `never`,
///    `without <verb>`, `just <answer|review|...>`, ...) override everything,
///    including execution keywords.
/// 2. **Question detection** — a leading interrogative or a trailing `?`.
/// 3. **Explicit outcome keywords** — `Verify`, then `Plan`, then `Review`,
///    then `Diagnose`, then `Execute` (priority order, first hit wins).
/// 4. **Small-talk corpus** — short greetings/pleasantries route to a
///    read-only `Answer` (bounded to short inputs, so a long message that
///    merely opens with a greeting still reaches `AskUser`).
/// 5. **AskUser** — nothing matched; the run asks the user.
///
/// This is the deterministic, offline chain (ADR-55). The orchestrator's LLM
/// classifier (ADR-56, enabled by default) is a wrapper around [`route()`]: it
/// runs `route()` first, then — when the classifier is enabled and the route
/// is not one of the two fast paths (negation-override and smalltalk) — asks
/// the model to re-classify the request. A classification at or above the
/// configured threshold replaces the deterministic result; below-threshold,
/// disabled, or fail-soft, this deterministic chain stands unchanged.
///
/// `hinted_paths` are extracted in every branch: quoted tokens, `./`, `../` or
/// `/`-prefixed tokens, and tokens ending in a known source extension are
/// joined to `project_dir`; any path whose lexical normalization escapes
/// `project_dir` is dropped. The function never touches the filesystem.
pub fn route(input: &str, project_dir: PathBuf) -> RouterOutput {
    let root = absolute_root(&project_dir);
    let scope = TaskScope {
        project_dir: project_dir.clone(),
        hinted_paths: extract_hinted_paths(input, &root),
    };
    let normalized = normalize_input(input);
    let tokens = tokens_of(&normalized);

    // Empty/whitespace-only input: nothing to route.
    if normalized.is_empty() {
        return RouterOutput {
            outcome: RequestedOutcome::Answer,
            scope,
            confidence: 0.0,
            route: RouterRoute::AskUser,
        };
    }

    // (a) Negation corpus first — wins over every other signal.
    if NEGATION_PHRASES.iter().any(|phrase| contains_standalone(&normalized, phrase)) {
        return RouterOutput {
            outcome: read_only_outcome(&normalized, &tokens),
            scope,
            confidence: 0.9,
            route: RouterRoute::RuleHit { rule: RULE_NEGATION_OVERRIDE },
        };
    }

    // (b) Question detection.
    if is_question(&normalized) {
        let outcome = if contains_any(&normalized, DIAGNOSE_WORDS) {
            RequestedOutcome::Diagnose
        } else {
            RequestedOutcome::Answer
        };
        return RouterOutput {
            outcome,
            scope,
            confidence: 0.9,
            route: RouterRoute::RuleHit { rule: RULE_QUESTION },
        };
    }

    // (c) Explicit outcome keywords in priority order (first hit wins).
    if let Some((outcome, rule)) = explicit_outcome_keyword(&tokens) {
        return RouterOutput {
            outcome,
            scope,
            confidence: 0.8,
            route: RouterRoute::RuleHit { rule },
        };
    }

    // (d) Small-talk corpus — short greetings/pleasantries ("hi", "thanks",
    //     "what's up", ...) route to a read-only Answer instead of the AskUser
    //     modal. Bounded to short inputs so a long message that happens to
    //     start with a greeting (and may hide a real intent) still falls
    //     through to the AskUser sink below.
    if normalized.chars().count() <= SMALLTALK_MAX_INPUT_LEN
        && SMALLTALK_PHRASES.iter().any(|phrase| contains_standalone(&normalized, phrase))
    {
        return RouterOutput {
            outcome: RequestedOutcome::Answer,
            scope,
            confidence: 0.9,
            route: RouterRoute::RuleHit { rule: RULE_SMALLTALK },
        };
    }

    // (e) No rule matched — ask the user. (When the orchestrator's LLM
    // classifier is enabled this AskUser result is the deterministic fallback
    // for a non-fast-path request; the wrapper may re-route it above
    // threshold.)
    RouterOutput {
        outcome: RequestedOutcome::Answer,
        scope,
        confidence: 0.0,
        route: RouterRoute::AskUser,
    }
}

/// `input` lowercased, surrounded whitespace stripped, and typographic
/// apostrophes normalized to ASCII so `don’t` matches the `don't` corpus.
fn normalize_input(input: &str) -> String {
    input.trim().to_lowercase().replace(['\u{2018}', '\u{2019}'], "'")
}

/// Read-only negation/limiting corpus (routing step a). Matched FIRST and with
/// priority over every other signal.
const NEGATION_PHRASES: &[&str] = &[
    "don't",
    "do not",
    "never",
    "shouldn't",
    "without touching",
    "without changing",
    "without modifying",
    "without fixing",
    "without editing",
    "without writing",
    "read-only",
    "just answer",
    "just explain",
    "just tell",
    "just describe",
    "just talk",
    "just review",
    "no changes",
    "don't touch",
];

/// Small-talk corpus (routing step d). Greetings, pleasantries, thanks,
/// goodbyes, and lightweight "who/what are you" openers route to a read-only
/// [`RequestedOutcome::Answer`] so plain chat ("hi, lets work on something")
/// does not fall into the AskUser confirmation modal.
///
/// Matched with [`contains_standalone`] on normalized input, like the negation
/// corpus. Deliberately EXCLUDES every plan-approval/continue signal (`ok`,
/// `okay`, `yes`, `sure`, `go`, `continue`, `proceed`, `approved`, `do it`,
/// `lets go`, `start`, `go ahead`) so the plan-approval arming paths are never
/// shadowed. Only fires when the normalized input is at most
/// [`SMALLTALK_MAX_INPUT_LEN`] chars: a long message that merely opens with a
/// greeting may hide a real intent and must fall through to AskUser.
const SMALLTALK_PHRASES: &[&str] = &[
    "hi",
    "hello",
    "hey",
    "yo",
    "howdy",
    "hi there",
    "hello there",
    "hey there",
    "whats up",
    "what's up",
    "how are you",
    "how are things",
    "how's it going",
    "good morning",
    "good afternoon",
    "good evening",
    "thanks",
    "thank you",
    "no problem",
    "you're welcome",
    "bye",
    "goodbye",
    "see you",
    "see you later",
    "who are you",
    "what can you do",
    "what can you help with",
    "nice to meet you",
    "how do you work",
    "what is your name",
    "lets work on something",
    "let's work on something",
];

/// Max length (in chars) of the normalized input for the small-talk corpus
/// (routing step d) to fire. Longer inputs fall through to AskUser even when
/// they open with a greeting — they may hide a real intent.
const SMALLTALK_MAX_INPUT_LEN: usize = 48;

/// Diagnostic/error wording used to elevate an answer to a diagnose (routing
/// steps b and the negation branch).
const DIAGNOSE_WORDS: &[&str] = &[
    "why",
    "what's wrong",
    "failing",
    "error",
    "crash",
    "panic",
    "debug",
    "diagnose",
    "traceback",
];

/// Read-only outcome for the negation branch: `Plan` when the user explicitly
/// asks for a plan (a read-only deliverable), then `Diagnose` when diagnostic
/// wording is present, then `Review` when review wording is present, otherwise
/// `Answer`.
///
/// Planning is elevated BEFORE the diagnose/review checks and above the
/// negation corpus's demotion because a plan is read-only by construction
/// (ADR-55 Phase 2b planning-only runs carry zero grants) — a plan request
/// like "plan the refactor but don't touch the parser" or a constraints-heavy
/// prompt ("...no external crates, no `unsafe`, don't use unwrap...") must
/// stay a Plan run, not be demoted to a text-only Diagnose/Answer merely
/// because it names restrictions. Execution/verify keywords are deliberately
/// ignored — the negation corpus means the user wants no changes, so the
/// execute keyword is dropped (e.g. "fix it but don't touch the parser").
///
/// Only the plan-family corpus elevates (not `design`/`architecture for`,
/// which commonly appear in "without changing the design of ..."). Diagnostic
/// wording (see [`DIAGNOSE_WORDS`]) retains its substring semantics; the
/// review keywords, shared with the explicit-outcome branch, are matched on
/// word boundaries.
fn read_only_outcome(normalized: &str, tokens: &[String]) -> RequestedOutcome {
    if contains_any_keyword(tokens, NEGATION_PLAN_KEYWORDS) {
        return RequestedOutcome::Plan;
    }
    if contains_any(normalized, DIAGNOSE_WORDS) {
        return RequestedOutcome::Diagnose;
    }
    if contains_any_keyword(tokens, REVIEW_KEYWORDS) {
        return RequestedOutcome::Review;
    }
    RequestedOutcome::Answer
}

/// Plan-family keywords that survive the negation override.
///
/// Deliberately narrower than [`PLAN_KEYWORDS`]: `design` and `architecture
/// for` are excluded because they routinely appear inside "without changing
/// the design/architecture of X", which is a read-only *critique* request,
/// not a planning request.
const NEGATION_PLAN_KEYWORDS: &[&str] =
    &["plan", "plans", "planning", "proposal", "proposals", "blueprint", "roadmap"];

/// Review keywords, matched on word boundaries. Explicit inflections
/// (`reviewing`, `critiquing`, `auditing`) are listed verbatim; past tense
/// (`reviewed`, `audited`) is deliberately excluded — see the module docs.
const REVIEW_KEYWORDS: &[&str] = &[
    "review",
    "reviews",
    "reviewing",
    "critique",
    "critiquing",
    "audit",
    "auditing",
    "code review",
    "check my",
    "look over",
];

/// Explicit outcome keywords, evaluated in the documented priority order
/// (first hit wins): Verify → Plan → Review → Diagnose → Execute.
///
/// `tokens` is the word-boundary tokenized input (see [`tokens_of`]); a
/// keyword fires only when it is a whole token or a run of consecutive
/// tokens, so benign words (`fixture`, `recreate`, `write-up`) cannot match
/// a keyword as a substring.
///
/// Returns the matched outcome plus the `RuleHit` rule-name constant for it.
fn explicit_outcome_keyword(tokens: &[String]) -> Option<(RequestedOutcome, &'static str)> {
    const VERIFY_KEYWORDS: &[&str] = &[
        "verify",
        "verifies",
        "verifying",
        "ensure",
        "ensures",
        "ensuring",
        "check that",
        "confirm that",
        "test that",
        "run the tests",
        "run tests",
        "run the test",
        "run the test suite",
        "run cargo test",
        "run the build",
    ];
    const PLAN_KEYWORDS: &[&str] = &[
        "plan",
        "plans",
        "planning",
        "design",
        "designs",
        "designing",
        "proposal",
        "blueprint",
        "architecture for",
        "roadmap",
    ];
    const DIAGNOSE_KEYWORDS: &[&str] = &[
        "why is",
        "what's wrong",
        "diagnose",
        "diagnosing",
        "debug",
        "debugging",
        "crash",
        "crashes",
        "crashing",
        "stack trace",
        "failing",
    ];
    const EXECUTE_KEYWORDS: &[&str] = &[
        "implement",
        "implementing",
        "fix",
        "fixes",
        "fixing",
        "add",
        "adds",
        "adding",
        "create",
        "creates",
        "creating",
        "write",
        "writes",
        "writing",
        "build",
        "builds",
        "building",
        "refactor",
        "refactoring",
        "update",
        "updates",
        "updating",
        "remove",
        "removes",
        "removing",
        "migrate",
        "migrating",
        "install",
        "installing",
        "execute",
        "run",
        "apply",
        "approve",
        "make it",
    ];

    for (keywords, outcome, rule) in [
        (&VERIFY_KEYWORDS, RequestedOutcome::Verify, RULE_VERIFY),
        (&PLAN_KEYWORDS, RequestedOutcome::Plan, RULE_PLAN),
        (&REVIEW_KEYWORDS, RequestedOutcome::Review, RULE_REVIEW),
        (&DIAGNOSE_KEYWORDS, RequestedOutcome::Diagnose, RULE_DIAGNOSE),
        (&EXECUTE_KEYWORDS, RequestedOutcome::Execute, RULE_EXECUTE),
    ] {
        if contains_any_keyword(tokens, keywords) {
            return Some((outcome, rule));
        }
    }
    None
}

/// Split `normalized` into whitespace-delimited whole words, stripping
/// leading/trailing non-alphanumeric characters from each token.
///
/// Internal punctuation — hyphens, underscores, apostrophes, slashes — stays
/// part of the token, so compounds like `write-up`, `fetch_loop`, `what's`
/// and `src/lib.rs` match as single units: `write-up` must not fire the
/// `write` keyword.
fn tokens_of(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let whole = token.trim_matches(|c: char| !c.is_alphanumeric());
            (!whole.is_empty()).then(|| whole.to_owned())
        })
        .collect()
}

/// True when `tokens` contains `keyword` as a sequence of whole tokens.
///
/// Single-word keywords match an exact token; multi-word keywords (phrases
/// like `run the tests`) match consecutive tokens. This is the
/// word-boundary matcher that implements the module's keyword policy.
fn contains_keyword(tokens: &[String], keyword: &str) -> bool {
    let words: Vec<&str> = keyword.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    if words.len() == 1 {
        return tokens.iter().any(|token| token == words[0]);
    }
    tokens
        .windows(words.len())
        .any(|window| window.iter().zip(&words).all(|(token, word)| token.as_str() == *word))
}

/// True when `tokens` contains any of `keywords` on a word boundary.
fn contains_any_keyword(tokens: &[String], keywords: &[&str]) -> bool {
    keywords.iter().any(|keyword| contains_keyword(tokens, keyword))
}

/// True when `text` contains any of `keywords` as a plain substring
/// (case-insensitive input is already normalized by the caller).
///
/// Retained only for the diagnostic-elevation corpus ([`DIAGNOSE_WORDS`]),
/// which keeps its original substring semantics by design (see module docs).
fn contains_any(text: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|keyword| text.contains(keyword))
}

/// True when `text` contains `phrase` as a standalone token/phrase — the
/// characters immediately before and after the match are non-alphanumeric or
/// absent. Used for the short negation words so `don't`/`never` do not match
/// inside unrelated words (e.g. "nevertheless").
fn contains_standalone(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(start, _)| {
        let end = start + phrase.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let is_boundary = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric());
        is_boundary(before) && is_boundary(after)
    })
}

/// Leading-interrogative or trailing-`?` question detection (routing step b).
fn is_question(normalized: &str) -> bool {
    if normalized.trim_end().ends_with('?') {
        return true;
    }
    leading_interrogative(normalized)
}

/// True when the first alphabetic word of `normalized` is a leading
/// interrogative (what/why/how/which/who/where/can/could/would/is/are/
/// does/do/when).
fn leading_interrogative(normalized: &str) -> bool {
    const INTERROGATIVES: &[&str] = &[
        "what", "why", "how", "which", "who", "where", "can", "could", "would", "is", "are",
        "does", "do", "when",
    ];
    let first_word = normalized.split(|c: char| !c.is_alphabetic()).find(|word| !word.is_empty());
    first_word.is_some_and(|word| INTERROGATIVES.contains(&word))
}

/// File extensions that mark a bare token as a likely file path.
const KNOWN_SOURCE_EXTENSIONS: &[&str] =
    &["rs", "toml", "md", "json", "lock", "sh", "yml", "yaml", "py", "ts", "js", "css", "html"];

/// Heuristically extract hinted file paths from a request string, normalized
/// to absolute paths within `root`.
///
/// Candidates: quoted tokens (always), `./`/`../`/`/`-prefixed tokens, and
/// tokens ending in a known source extension. Pure and deterministic — lexical
/// normalization only, mirroring the containment style used by the tools
/// crate's `resolve_path` (which we cannot reuse: `core` must not depend on
/// `tools`).
fn extract_hinted_paths(input: &str, root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for (token, quoted) in tokenize(input) {
        let cleaned = clean_token(&token);
        if !quoted && !is_path_candidate(&cleaned) {
            continue;
        }
        let joined = if Path::new(&cleaned).is_absolute() {
            PathBuf::from(&cleaned)
        } else {
            root.join(&cleaned)
        };
        let Some(normalized) = normalize_within(root, &joined) else {
            continue;
        };
        if !paths.contains(&normalized) {
            paths.push(normalized);
        }
    }
    paths
}

/// Tokenize `input` into (token, was_quoted) pairs. A `"..."` or `'...'` span
/// is returned whole (quotes stripped); everything else is split on runs of
/// whitespace/quotes. Surrounded punctuation is left for [`clean_token`].
fn tokenize(input: &str) -> Vec<(String, bool)> {
    let mut tokens = Vec::new();
    let mut it = input.char_indices().peekable();
    while let Some(&(_, c)) = it.peek() {
        if c.is_whitespace() {
            it.next();
            continue;
        }
        if c == '"' || c == '\'' {
            it.next();
            let mut quoted = String::new();
            for (_, q) in it.by_ref() {
                if q == c {
                    break;
                }
                quoted.push(q);
            }
            let trimmed = quoted.trim().to_string();
            if !trimmed.is_empty() {
                tokens.push((trimmed, true));
            }
            continue;
        }
        let mut word = String::new();
        while let Some(&(_, c)) = it.peek() {
            if c.is_whitespace() || c == '"' || c == '\'' {
                break;
            }
            word.push(c);
            it.next();
        }
        tokens.push((word, false));
    }
    tokens
}

/// Strip surrounding punctuation from a non-quoted token so candidate testing
/// and joining operate on `README.md`, not `README.md,`. A leading `./` is
/// preserved because it is a path marker, not decoration.
fn clean_token(token: &str) -> String {
    let leading = if token.starts_with("./") {
        token
    } else {
        token.trim_start_matches(|c: char| ".,;:!?()[]{}".contains(c))
    };
    leading.trim_end_matches(|c: char| ".,;:!?()[]{}".contains(c)).to_string()
}

/// True when a cleaned token looks like a path: an explicit `./`, `../` or `/`
/// marker, or a known source-extension suffix.
fn is_path_candidate(token: &str) -> bool {
    if token.starts_with("./") || token.starts_with("../") || token.starts_with('/') {
        return true;
    }
    let lower = token.to_ascii_lowercase();
    KNOWN_SOURCE_EXTENSIONS.iter().any(|ext| lower.ends_with(&format!(".{ext}")))
}

/// Lexically collapse `.`/`..` components (no filesystem access) and return
/// the path only when it still lies within `root`. `None` when the candidate
/// escapes the project root (path-traversal containment).
fn normalize_within(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.starts_with(root) {
        Some(normalized)
    } else {
        None
    }
}

/// Make `path` absolute lexically so `hinted_paths` are always absolute and
/// containment checks are robust against a relative project root.
fn absolute_root(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Resolve a serialized rule name back to the matching `'static` constant so
/// [`RouterRoute`] can round-trip with a `&'static str` field (the serde
/// derive cannot deserialize `&'static str` directly).
fn rule_name_to_static(rule: &str) -> Option<&'static str> {
    match rule {
        RULE_NEGATION_OVERRIDE => Some(RULE_NEGATION_OVERRIDE),
        RULE_QUESTION => Some(RULE_QUESTION),
        RULE_VERIFY => Some(RULE_VERIFY),
        RULE_PLAN => Some(RULE_PLAN),
        RULE_REVIEW => Some(RULE_REVIEW),
        RULE_DIAGNOSE => Some(RULE_DIAGNOSE),
        RULE_EXECUTE => Some(RULE_EXECUTE),
        RULE_SMALLTALK => Some(RULE_SMALLTALK),
        _ => None,
    }
}

impl Serialize for RouterRoute {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStructVariant;
        match self {
            // Externally-tagged representation matching what the serde derive
            // would emit: `{"RuleHit":{"rule":"..."}}`.
            RouterRoute::RuleHit { rule } => {
                let mut state =
                    serializer.serialize_struct_variant("RouterRoute", 0, "RuleHit", 1)?;
                state.serialize_field("rule", rule)?;
                state.end()
            }
            RouterRoute::LlmClassifier => {
                serializer.serialize_unit_variant("RouterRoute", 1, "LlmClassifier")
            }
            RouterRoute::AskUser => serializer.serialize_unit_variant("RouterRoute", 2, "AskUser"),
        }
    }
}

impl<'de> Deserialize<'de> for RouterRoute {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        // Parse the externally-tagged representation produced by the
        // `Serialize` impl: `"AskUser"` / `"LlmClassifier"` for unit variants
        // and `{"RuleHit":{"rule":"..."}}` for the struct variant. `rule`
        // arrives as an owned string and is mapped back onto the closed set of
        // `'static` rule-name constants.
        let tagged = serde_json::Value::deserialize(deserializer)?;
        match tagged {
            serde_json::Value::String(variant) => match variant.as_str() {
                "LlmClassifier" => Ok(RouterRoute::LlmClassifier),
                "AskUser" => Ok(RouterRoute::AskUser),
                other => {
                    Err(D::Error::custom(format!("unknown RouterRoute unit variant: {other}")))
                }
            },
            serde_json::Value::Object(map) => {
                let Some((name, inner)) = map.into_iter().next() else {
                    return Err(D::Error::custom("RouterRoute object must not be empty"));
                };
                if name != "RuleHit" {
                    return Err(D::Error::custom(format!("unknown RouterRoute variant: {name}")));
                }
                let Some(rule) = inner.get("rule").and_then(serde_json::Value::as_str) else {
                    return Err(D::Error::custom("RuleHit is missing a string 'rule' field"));
                };
                let rule = rule_name_to_static(rule)
                    .ok_or_else(|| D::Error::custom(format!("unknown router rule: {rule}")))?;
                Ok(RouterRoute::RuleHit { rule })
            }
            _ => Err(D::Error::custom(
                "unexpected RouterRoute shape; expected a variant name or a single-key object",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase-0 tests never touch the filesystem, so the project root is a
    /// deterministic temp path rather than a created directory.
    fn project() -> PathBuf {
        std::env::temp_dir().join("concerto-intent-tests")
    }

    fn hinted(out: &RouterOutput) -> &[PathBuf] {
        &out.scope.hinted_paths
    }

    #[test]
    fn negation_overrides_execute_keywords() {
        let out = route("implement the endpoint but don't touch the parser", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "negation_override" }));
    }

    #[test]
    fn negation_with_diagnose_words_routes_to_diagnose() {
        let out = route("without changing anything, explain why the tests crash", project());
        assert_eq!(out.outcome, RequestedOutcome::Diagnose);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "negation_override" }));
    }

    #[test]
    fn negation_with_plan_keyword_is_not_demoted() {
        let out = route("plan the refactor but don't touch the parser", project());
        assert_eq!(out.outcome, RequestedOutcome::Plan);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "negation_override" }));
    }

    #[test]
    fn constraint_heavy_plan_prompt_stays_plan_despite_diagnose_words() {
        // Verdict-style spec: negations ("don't") route through the read-only
        // branch, and the word "error" alone would elevate to Diagnose — the
        // explicit plan request must win because planning is read-only.
        let out = route(
            "plan: build a minimal cli tool with zero external crates, no unsafe, \
             don't use unwrap or expect, exit code 2 = error",
            project(),
        );
        assert_eq!(out.outcome, RequestedOutcome::Plan);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "negation_override" }));
    }

    #[test]
    fn negation_with_design_word_stays_review_not_plan() {
        // "design" is deliberately NOT part of the negation-plan corpus:
        // "without changing the design" is a read-only critique, not planning.
        let out = route("without changing the design, critique this module", project());
        assert_eq!(out.outcome, RequestedOutcome::Review);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "negation_override" }));
    }

    #[test]
    fn question_routes_to_answer() {
        let out = route("What is the fastest way to sort a list?", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "question" }));
    }

    #[test]
    fn why_question_routes_to_diagnose() {
        let out = route("Why is the build failing?", project());
        assert_eq!(out.outcome, RequestedOutcome::Diagnose);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "question" }));
    }

    #[test]
    fn verify_keyword_routes_to_verify() {
        let out = route("please verify the release build", project());
        assert_eq!(out.outcome, RequestedOutcome::Verify);
        assert_eq!(out.confidence, 0.8);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "verify_keyword" }));
    }

    #[test]
    fn plan_keyword_routes_to_plan() {
        let out = route("draft a plan for the refactor", project());
        assert_eq!(out.outcome, RequestedOutcome::Plan);
        assert_eq!(out.confidence, 0.8);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "plan_keyword" }));
    }

    #[test]
    fn review_keyword_routes_to_review() {
        let out = route("give me a code review of this change", project());
        assert_eq!(out.outcome, RequestedOutcome::Review);
        assert_eq!(out.confidence, 0.8);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "review_keyword" }));
    }

    #[test]
    fn diagnose_keyword_routes_to_diagnose() {
        let out = route("diagnose this crash", project());
        assert_eq!(out.outcome, RequestedOutcome::Diagnose);
        assert_eq!(out.confidence, 0.8);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "diagnose_keyword" }));
    }

    #[test]
    fn execute_keyword_routes_to_execute() {
        let out = route("implement the new endpoint", project());
        assert_eq!(out.outcome, RequestedOutcome::Execute);
        assert_eq!(out.confidence, 0.8);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }));
    }

    /// ADR-55 §12: bare directive words ("execute", "run", "apply",
    /// "approve") route as a confident Execute — they are the natural
    /// follow-up to a rendered plan. Without this, the router's AskUser
    /// sink showed the generic "I could not confidently tell what you want"
    /// list modal instead of the stored-plan dialog (live rounds 4/5).
    #[test]
    fn bare_execute_directives_route_to_execute() {
        for input in ["execute", "run", "apply", "approve"] {
            let out = route(input, project());
            assert_eq!(out.outcome, RequestedOutcome::Execute, "input: {input}");
            assert_eq!(out.confidence, 0.8, "input: {input}");
            assert!(
                matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }),
                "input: {input}"
            );
        }
    }

    /// ADR-55 §12: bare directives never override the negation corpus
    /// ("don't execute", "don't approve") or the read-only verify family
    /// ("run the tests" stays Verify; "run the plan" stays Plan).
    #[test]
    fn bare_directives_keep_priority_and_negation_semantics() {
        for input in ["don't execute anything", "don't run", "do not approve", "never apply"] {
            let out = route(input, project());
            assert_eq!(out.outcome, RequestedOutcome::Answer, "input: {input}");
            assert_eq!(out.confidence, 0.9, "input: {input}");
            assert!(
                matches!(out.route, RouterRoute::RuleHit { rule: "negation_override" }),
                "input: {input}"
            );
        }

        let out = route("run the tests", project());
        assert_eq!(out.outcome, RequestedOutcome::Verify);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "verify_keyword" }));

        let out = route("run tests", project());
        assert_eq!(out.outcome, RequestedOutcome::Verify, "run tests is a verify phrase");

        let out = route("run cargo test", project());
        assert_eq!(out.outcome, RequestedOutcome::Verify, "run cargo test is a verify phrase");

        let out = route("run the plan", project());
        assert_eq!(out.outcome, RequestedOutcome::Plan);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "plan_keyword" }));

        let out = route("apply the fix", project());
        assert_eq!(out.outcome, RequestedOutcome::Execute);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }));
    }

    /// ADR-55 §12: the new directive keywords match on exact word boundaries
    /// only — compounds and inflections ("runner", "running", "runbook",
    /// "runway", "application", "applying", "approving", "approved") must not
    /// fire the Execute rule.
    #[test]
    fn directive_compounds_and_inflections_never_route_to_execute() {
        for input in [
            "the runner is slow",
            "the app is running fine",
            "read the runbook first",
            "check the runway",
            "the application boots",
            "applying the patch failed",
            "approving this takes a day",
            "it was approved yesterday",
        ] {
            let out = route(input, project());
            assert_eq!(
                out.outcome,
                RequestedOutcome::Answer,
                "input: {input} must not route Execute"
            );
            assert!(matches!(out.route, RouterRoute::AskUser), "input: {input}");
        }
    }

    #[test]
    fn priority_order_plan_beats_execute_when_both_present() {
        let out = route("plan and then implement the feature", project());
        assert_eq!(out.outcome, RequestedOutcome::Plan);
        assert_eq!(out.confidence, 0.8);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "plan_keyword" }));
    }

    #[test]
    fn verify_beats_plan_when_both_present() {
        let out = route("verify the plan is sound", project());
        assert_eq!(out.outcome, RequestedOutcome::Verify);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "verify_keyword" }));
    }

    #[test]
    fn path_extraction_covers_quoted_relative_and_extension_tokens() {
        let root = project();
        let out = route(
            "read \"src/lib.rs\" and review ./README.md and src/main.rs and Cargo.toml",
            root.clone(),
        );
        let mut got = hinted(&out).to_vec();
        got.sort();
        let mut expected = vec![
            root.join("src/lib.rs"),
            root.join("README.md"),
            root.join("src/main.rs"),
            root.join("Cargo.toml"),
        ];
        expected.sort();
        assert_eq!(got, expected);
    }

    #[test]
    fn out_of_project_paths_are_dropped() {
        let root = project();
        let out = route("review /etc/passwd and ../outside and src/main.rs", root.clone());
        assert_eq!(hinted(&out), &[root.join("src/main.rs")]);
    }

    #[test]
    fn quoted_tokens_are_hinted_paths_even_without_extension() {
        let root = project();
        let out = route("please look at \"the-folder\" for me", root.clone());
        let expected = root.join("the-folder");
        assert_eq!(hinted(&out), &[expected]);
    }

    #[test]
    fn ambiguous_input_routes_to_ask_user_with_zero_confidence() {
        // "hmm" is genuinely ambiguous — greetings like "hello there" now
        // route to the smalltalk rule, so they no longer exercise AskUser.
        let out = route("hmm", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert_eq!(out.confidence, 0.0);
        assert!(matches!(out.route, RouterRoute::AskUser));
        assert!(!matches!(out.route, RouterRoute::RuleHit { .. }));
    }

    #[test]
    fn rule_outcomes_meet_or_exceed_low_confidence_threshold() {
        // A rule- or negation-derived outcome always beats the ask threshold;
        // an AskUser sink never meets it.
        for input in [
            "fix the bug",              // execute keyword → 0.8
            "Why is this failing?",     // question → 0.9
            "just describe the layout", // negation → 0.9
        ] {
            let out = route(input, project());
            assert!(
                out.confidence >= LOW_CONFIDENCE_THRESHOLD,
                "input {input:?} produced confidence {}",
                out.confidence
            );
        }
        // "hmm" is genuinely ambiguous; greetings like "hello" now hit the
        // smalltalk rule at confidence 0.9, so they no longer exercise the
        // AskUser sink.
        let ambiguous = route("hmm", project());
        assert!(ambiguous.confidence < LOW_CONFIDENCE_THRESHOLD);
        assert!(matches!(ambiguous.route, RouterRoute::AskUser));
    }

    // ------------------------------------------------------------------
    // Small-talk corpus (routing step d). Short greetings/pleasantries route
    // to a read-only Answer; the rule sits AFTER negation, questions, and
    // explicit keywords, and BEFORE the AskUser sink. Answer never grants
    // Execute, so this does not weaken the security gate.
    // ------------------------------------------------------------------

    #[test]
    fn smalltalk_greeting_routes_to_answer_without_askuser() {
        let out = route("hi", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "smalltalk" }));
    }

    #[test]
    fn smalltalk_work_opener_routes_to_answer() {
        let out = route("hi, lets work on something", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "smalltalk" }));
    }

    #[test]
    fn smalltalk_never_overrides_explicit_keywords() {
        let out = route("hello, build a calculator", project());
        assert_eq!(out.outcome, RequestedOutcome::Execute);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }));
        assert!(!matches!(out.route, RouterRoute::RuleHit { rule: "smalltalk" }));
    }

    #[test]
    fn smalltalk_never_overrides_negation() {
        let out = route("hi, don't touch anything, just answer", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "negation_override" }));
        assert!(!matches!(out.route, RouterRoute::RuleHit { rule: "smalltalk" }));
    }

    #[test]
    fn smalltalk_never_overrides_question() {
        let out = route("hi, what is the capital of France?", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "question" }));
        assert!(!matches!(out.route, RouterRoute::RuleHit { rule: "smalltalk" }));
    }

    #[test]
    fn smalltalk_requires_short_input() {
        // > 48 chars, opens with a greeting, and contains no intent keyword:
        // the length guard sends it to AskUser instead of the small-talk rule.
        let long = "hi, I am just saying hello in a very long winded conversational \
                    message that contains no intent keywords at all";
        assert!(long.chars().count() > 48, "fixture must exceed the smalltalk length guard");
        let out = route(long, project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert_eq!(out.confidence, 0.0);
        assert!(matches!(out.route, RouterRoute::AskUser));
    }

    #[test]
    fn approval_words_are_not_smalltalk() {
        // Plan-approval/continue signals must never be shadowed by the
        // small-talk corpus — they keep falling through to AskUser.
        for input in ["ok", "yes", "go ahead"] {
            let out = route(input, project());
            assert!(matches!(out.route, RouterRoute::AskUser), "input: {input}");
            assert!(
                !matches!(out.route, RouterRoute::RuleHit { rule: "smalltalk" }),
                "input: {input}"
            );
        }
    }

    #[test]
    fn routing_is_case_insensitive() {
        let lower = route("please verify the build", project());
        let upper = route("PLEASE VERIFY THE BUILD", project());
        assert_eq!(lower.outcome, upper.outcome);
        assert_eq!(lower.route, upper.route);
        assert_eq!(lower.confidence, upper.confidence);
    }

    #[test]
    fn empty_or_whitespace_input_routes_to_ask_user() {
        for input in ["", "   ", "\t\n "] {
            let out = route(input, project());
            assert!(matches!(out.route, RouterRoute::AskUser));
            assert_eq!(out.confidence, 0.0);
            assert_eq!(out.outcome, RequestedOutcome::Answer);
            assert!(hinted(&out).is_empty(), "no hinted paths from empty input");
        }
    }

    #[test]
    fn router_route_serializes_and_deserializes() {
        for expected in [
            RouterRoute::RuleHit { rule: "negation_override" },
            RouterRoute::RuleHit { rule: "smalltalk" },
            RouterRoute::LlmClassifier,
            RouterRoute::AskUser,
        ] {
            let json = serde_json::to_string(&expected).expect("RouterRoute serializes");
            let back: RouterRoute = serde_json::from_str(&json).expect("RouterRoute deserializes");
            assert_eq!(back, expected, "round-trip failed for {expected:?}");
        }
    }

    #[test]
    fn router_output_serializes_with_route_and_scope() {
        let out = route("review src/main.rs", project());
        let json = serde_json::to_string(&out).expect("RouterOutput serializes");
        let back: RouterOutput = serde_json::from_str(&json).expect("RouterOutput deserializes");
        assert_eq!(back.outcome, out.outcome);
        assert_eq!(back.confidence, out.confidence);
        assert_eq!(back.route, out.route);
        assert_eq!(back.scope, out.scope);
    }

    #[test]
    fn run_stage_display_is_enum_name() {
        for (stage, name) in [
            (RunStage::Understand, "Understand"),
            (RunStage::Inspect, "Inspect"),
            (RunStage::Plan, "Plan"),
            (RunStage::Execute, "Execute"),
            (RunStage::Verify, "Verify"),
            (RunStage::Complete, "Complete"),
        ] {
            assert_eq!(stage.to_string(), name);
        }
    }

    /// ADR-55 Phase 2c §7 / ADR-56: `llm_classifier_is_never_produced_in_phase_0`
    /// is **superseded**. The replacement contract:
    /// `RouterRoute::LlmClassifier` is produced **only** via the classifier
    /// path — a wrapper around `route()` mounted after the two fast paths in
    /// `concerto-orchestrator` (`intent_classifier` + `runtime_runner` wiring)
    /// — never by `route()` itself.
    ///
    /// This test keeps assertion (a): `route()` over the corpus never yields
    /// `RouterRoute::LlmClassifier`. Assertion (b) — the wrapper yields it
    /// exactly when re-routing a deterministic result above threshold — lives
    /// in the orchestrator's intent-classifier tests.
    #[test]
    fn llm_classifier_is_only_produced_by_the_2c_wrapper() {
        // (a) `route()` over the corpus never yields LlmClassifier: every
        // routing branch is rule- or ask-based; the classifier label must
        // never show up from the pure router itself.
        let route_of = |input: &str| route(input, project()).route;
        for input in [
            "implement the endpoint but don't touch the parser",
            "What is the fastest way to sort a list?",
            "diagnose this crash",
            "hello there",
        ] {
            assert!(!matches!(route_of(input), RouterRoute::LlmClassifier));
        }
    }

    // ------------------------------------------------------------------
    // Word-boundary keyword matching regression set (batch 1c).
    // Keywords fire only on whole-token boundaries; benign words that merely
    // *contain* a keyword no longer false-route to Execute.
    // ------------------------------------------------------------------

    /// Benign words that contain an Execute keyword as a substring must not
    /// route to Execute: `fixture` contains `fix`, `recreate` contains
    /// `create`, `additionally` contains `add`.
    #[test]
    fn benign_substring_words_do_not_route_to_execute() {
        for input in ["explain the fixture", "recreate", "additionally"] {
            let out = route(input, project());
            assert!(
                !matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }),
                "input {input:?} must not route to execute_keyword, got {:?}",
                out.route
            );
        }
        // The clean, ask-based outcome for a request with no other signal.
        let out = route("explain the fixture", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert!(matches!(out.route, RouterRoute::AskUser));
    }

    /// A hyphenated compound noun is a single token, so `write-up` must not
    /// fire the `write` keyword.
    #[test]
    fn hyphenated_compound_is_not_execute() {
        let out = route("write-up", project());
        assert!(!matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }));
        assert!(matches!(out.route, RouterRoute::AskUser));
    }

    /// Negation still wins over everything, including a word that would not
    /// even match a keyword on its own: `don't recreate` stays on the
    /// read-only negation path.
    #[test]
    fn negated_recreate_stays_on_read_only_path() {
        let out = route("don't recreate", project());
        assert_eq!(out.outcome, RequestedOutcome::Answer);
        assert_eq!(out.confidence, 0.9);
        assert!(matches!(out.route, RouterRoute::RuleHit { rule: "negation_override" }));
        assert!(!matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }));
    }

    /// Positive controls: the canonical Execute requests still hit the
    /// execute rule at confidence 0.8.
    #[test]
    fn execute_keywords_still_route_to_execute() {
        for input in [
            "implement a parser",
            "fix the bug",
            "add tests",
            "refactor fetch_loop",
            "create the module",
        ] {
            let out = route(input, project());
            assert_eq!(
                out.outcome,
                RequestedOutcome::Execute,
                "input {input:?} must route to Execute"
            );
            assert_eq!(out.confidence, 0.8, "input {input:?} must carry confidence 0.8");
            assert!(matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }));
        }
    }

    /// Inflected forms the corpus intends are listed verbatim (no stemming):
    /// third-person singular and gerund forms still route to Execute.
    #[test]
    fn explicit_inflections_route_to_execute() {
        for input in ["writes a test", "writing a test", "fixing the bug", "updating the docs"] {
            let out = route(input, project());
            assert_eq!(
                out.outcome,
                RequestedOutcome::Execute,
                "input {input:?} must route to Execute"
            );
            assert!(matches!(out.route, RouterRoute::RuleHit { rule: "execute_keyword" }));
        }
    }

    /// Positive controls for the read-only outcome rules.
    #[test]
    fn verify_review_and_diagnose_keywords_still_route() {
        for (input, expected, rule) in [
            ("verify the change", RequestedOutcome::Verify, "verify_keyword"),
            ("run the tests", RequestedOutcome::Verify, "verify_keyword"),
            ("review the code", RequestedOutcome::Review, "review_keyword"),
            ("why is it failing", RequestedOutcome::Diagnose, "question"),
        ] {
            let out = route(input, project());
            assert_eq!(out.outcome, expected, "input {input:?} must route to {expected:?}");
            assert!(matches!(out.route, RouterRoute::RuleHit { rule: r } if r == rule));
        }
    }
}
