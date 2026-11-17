//! Interpret completed test commands before truncating their display output.

use concerto_core::types::{EvalProvenance, EvalResult, TestRunner};

pub(super) fn completed_result(
    runner: TestRunner,
    exit_code: i32,
    duration_ms: u64,
    stdout: &str,
    stderr: &str,
    complete: bool,
) -> EvalResult {
    let mut combined = format!("{stdout}\n{stderr}");
    let confirmed = runner != TestRunner::Cargo || cargo_tests_passed(&combined);
    if exit_code == 0 && !confirmed {
        combined.push_str(
            "\nverification failed: Rust test success was not confirmed; require at least one \
             passed test and no failed or malformed libtest summaries. Empty, ignored-only, \
             missing or unsupported results cannot establish a pass.",
        );
    }
    if !complete {
        combined.push_str(
            "\nverification failed: process output capture was incomplete (a stream exceeded \
             the capture limit or a pipe read failed); an exit code and a passing prefix cannot \
             certify the test run.",
        );
    }
    // Inspect all available evidence first; a later empty doc-test group or
    // long diagnostic must not erase earlier executed tests. Count characters
    // when bounding the display tail so Unicode output cannot panic.
    let output_tail = combined.chars().rev().take(2000).collect::<String>().chars().rev().collect();
    EvalResult {
        runner,
        exit_code,
        passed: exit_code == 0 && confirmed && complete,
        duration_ms,
        output_tail,
        coverage: None,
        incomplete: !complete,
        provenance: EvalProvenance::Harness,
    }
}

/// Fail closed for unknown/custom harness formats. An exit code alone cannot
/// prove that tests ran. Empty groups are normal alongside nonempty unit,
/// integration or doc-test groups; ignored/measured/filtered tests do not count.
fn cargo_tests_passed(output: &str) -> bool {
    let mut any_passed = false;
    for summary in output.lines().filter_map(|line| line.strip_prefix("test result: ")) {
        let Some(passed) = passed_count(summary) else {
            return false;
        };
        any_passed |= passed > 0;
    }
    any_passed
}

fn passed_count(summary: &str) -> Option<u64> {
    let mut fields = summary.strip_prefix("ok. ")?.split(';').map(str::trim);
    let passed = fields.next()?.strip_suffix(" passed")?.parse::<u64>().ok()?;
    if fields.next()?.strip_suffix(" failed")?.parse::<u64>().ok()? != 0 {
        return None;
    }
    for suffix in [" ignored", " measured", " filtered out"] {
        fields.next()?.strip_suffix(suffix)?.parse::<u64>().ok()?;
    }
    let seconds =
        fields.next()?.strip_prefix("finished in ")?.strip_suffix('s')?.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds < 0.0 || fields.next().is_some() {
        return None;
    }
    Some(passed)
}

#[cfg(test)]
mod tests {
    use super::completed_result;
    use concerto_core::types::TestRunner;

    const PASS: &str = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; \
                        0 filtered out; finished in 0.00s\n";

    #[test]
    fn incomplete_capture_cannot_pass_with_a_passing_prefix() {
        // This is what the weak process path returns for H34: the passing
        // summary and filler survive, the later failing summary was discarded.
        let captured = format!("{PASS}{}", "a".repeat(64));

        let incomplete = completed_result(TestRunner::Cargo, 0, 5, &captured, "", false);
        assert!(!incomplete.passed, "incomplete evidence cannot certify a pass");
        assert!(incomplete.incomplete);
        assert!(
            incomplete.output_tail.contains("capture was incomplete"),
            "the verdict must explain the incomplete evidence: {}",
            incomplete.output_tail
        );

        let complete = completed_result(TestRunner::Cargo, 0, 5, &captured, "", true);
        assert!(complete.passed, "the identical complete text still passes");
        assert!(!complete.incomplete);
        assert!(!complete.output_tail.contains("capture was incomplete"));
    }
}
