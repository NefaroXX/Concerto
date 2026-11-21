"""Isolated counterexamples for the exact reviewed function bodies.

This is not an end-to-end harness test. Message is a minimal field-compatible
stand-in: the production trimming function reads only tool_calls. The path and
agent-id validation functions use their unchanged production bodies.
Generated Rust and its binary stay in the main repository's disk-backed target.
Expected result at fae4e80: three assertion failures (exit 101).
"""

from pathlib import Path
import hashlib
import json
import subprocess
import sys


def function(source: str, signature: str) -> str:
    start = source.index(signature)
    return source[start : source.index("\n}", start) + 2]


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "/home/sol/Projects/Concerto-worktrees/h40")
    expected = "fae4e80d4ec7a3f79323989598801f11cafaa058"
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    if head != expected:
        raise SystemExit(f"Expected reviewed head {expected}, found {head}")
    out = Path(__file__).resolve().parents[2] / "target/harness-review-2026-11-21"
    out.mkdir(parents=True, exist_ok=True)
    history_path = root / "crates/orchestrator/src/supervised_history.rs"
    config_path = root / "crates/config/src/agents.rs"
    history = history_path.read_text()
    config = config_path.read_text()
    extracted = [
        function(history, "pub fn paths_for("),
        function(history, "fn sanitize_agent_id("),
        function(history, "pub fn trim_unanswered_tool_call_tail("),
        function(config, "fn validate_agent_id("),
    ]
    support = r'''
use std::path::{Path, PathBuf};
#[derive(Debug, PartialEq, Eq)]
pub struct SupervisedHistoryPaths { pub history: PathBuf, pub metrics: PathBuf }
#[derive(Debug)]
pub enum ConfigError { InvalidValue(String) }
pub struct Message {
    pub tool_calls: Option<Vec<&'static str>>,
    pub tool_results: Vec<&'static str>,
}
fn assistant() -> Message {
    Message { tool_calls: Some(vec!["call-a", "call-b"]), tool_results: vec![] }
}
fn first_result() -> Message {
    Message { tool_calls: None, tool_results: vec!["call-a"] }
}
fn has_unanswered_call(messages: &[Message]) -> bool {
    messages.iter().filter_map(|m| m.tool_calls.as_ref()).flatten().any(|id| {
        !messages.iter().any(|m| m.tool_results.contains(id))
    })
}
#[test]
fn distinct_valid_agent_ids_must_have_distinct_files() {
    assert!(validate_agent_id("review.v1").is_ok());
    assert!(validate_agent_id("review_v1").is_ok());
    assert_ne!(paths_for(Path::new("fixture"), "review.v1"),
               paths_for(Path::new("fixture"), "review_v1"));
}
#[test]
fn partial_multi_tool_batch_must_be_replay_safe() {
    let mut messages = vec![assistant(), first_result()];
    trim_unanswered_tool_call_tail(&mut messages);
    assert!(!has_unanswered_call(&messages), "call-b still lacks a response");
}
#[test]
fn runtime_observation_after_partial_batch_must_not_hide_missing_result() {
    let observation = Message { tool_calls: None, tool_results: vec![] };
    let mut messages = vec![assistant(), first_result(), observation];
    trim_unanswered_tool_call_tail(&mut messages);
    assert!(!has_unanswered_call(&messages), "terminal observation hides unresolved call-b");
}
'''
    source = out / "review_probes.rs"
    source.write_text(support + "\n" + "\n\n".join(extracted))
    manifest = {
        "reviewed_head": head,
        "source_sha256": {
            str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (history_path, config_path)
        },
        "extracted_bodies_sha256": hashlib.sha256("\n\n".join(extracted).encode()).hexdigest(),
        "scope": "isolated production function bodies; minimal Message stand-in; not integration",
    }
    (out / "source.json").write_text(json.dumps(manifest, indent=2) + "\n")
    binary = out / "review_probes"
    subprocess.run(
        ["rustc", "--edition=2021", "--test", "-D", "warnings", str(source), "-o", str(binary)],
        cwd=root,
        check=True,
    )
    result = subprocess.run([str(binary), "--nocapture"], text=True, capture_output=True)
    output = result.stdout + result.stderr
    (out / "results.log").write_text(output)
    print(output)
    print(f"Probe exit status: {result.returncode}")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
