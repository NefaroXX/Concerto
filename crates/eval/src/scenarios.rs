use concerto_core::types::TaskId;

/// A single bug-injection test case.
///
/// Each case defines a task description and a set of bug patterns that a
/// ReviewerAgent (or equivalent) should catch. The mock provider returns the
/// `provider_response` which contains one or more of the `bug_patterns`; the
/// benchmark asserts that the agent flags at least one of them.
#[derive(Debug, Clone)]
pub struct BugInjectionCase {
    pub id: TaskId,
    pub description: &'static str,
    /// Code snippet or diff text that the mock "coder" returns.
    pub provider_response: &'static str,
    /// Patterns the reviewer MUST detect in the response.
    pub bug_patterns: &'static [&'static str],
    /// If true, the reviewer is expected to miss the bug (for negative tests).
    pub should_catch: bool,
}

/// A benchmark scenario describes a multi-agent task to run.
///
/// Scenarios are instantiated with mock providers that return deterministic
/// responses so the test is reproducible and fast.
#[derive(Debug, Clone)]
pub struct BenchmarkScenario {
    pub id: TaskId,
    pub name: &'static str,
    pub description: &'static str,
    /// Agent task description fed to the coordinator.
    pub agent_task_description: &'static str,
    /// Bug-injection cases that apply to this scenario.
    pub bug_cases: Vec<BugInjectionCase>,
}

// ---------------------------------------------------------------------------
// Scenario A: Add a feature
// ---------------------------------------------------------------------------
//
// A task that requires design + implementation + tests.  The mock architect
// produces a design doc; the mock coder returns code with a subtle logic
// error (off-by-one in a loop bound); the reviewer is expected to catch it.

pub fn scenario_a_add_feature() -> BenchmarkScenario {
    BenchmarkScenario {
        id: TaskId::new(),
        name: "add-feature",
        description: "Add a new `/healthz` endpoint that returns service status",
        agent_task_description: "Implement a health-check endpoint returning \
            JSON with service name, version, and uptime",
        bug_cases: vec![
            BugInjectionCase {
                id: TaskId::new(),
                description: "Off-by-one in uptime calculation",
                provider_response: "let uptime_secs = started_at.elapsed().as_secs() + 1;",
                bug_patterns: &["off-by-one", "uptime", "elapsed"],
                should_catch: true,
            },
            BugInjectionCase {
                id: TaskId::new(),
                description: "Misspelled JSON field (healhz instead of healthz)",
                provider_response: r#"{"status":"ok","healhz":true}"#,
                bug_patterns: &["healhz", "typo", "misspell"],
                should_catch: true,
            },
            BugInjectionCase {
                id: TaskId::new(),
                description: "Missing Content-Type header in response",
                provider_response: "fn healthz() -> Response {\n    Response::new(200, \"ok\")\n}",
                bug_patterns: &["content-type", "header", "content_type"],
                should_catch: true,
            },
            BugInjectionCase {
                id: TaskId::new(),
                description: "No unit test added for the new endpoint",
                provider_response: "// TODO: add tests later",
                bug_patterns: &["test", "unittest", "no test"],
                should_catch: true,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Scenario B: Refactor a module
// ---------------------------------------------------------------------------
//
// The mock architect produces a refactoring plan; the mock coder returns code
// that duplicates logic instead of extracting it.  The reviewer is expected to
// flag the duplication and the missing abstraction.

pub fn scenario_b_refactor_module() -> BenchmarkScenario {
    BenchmarkScenario {
        id: TaskId::new(),
        name: "refactor-module",
        description: "Extract shared validation logic from two handlers into a helper",
        agent_task_description: "Refactor the login and register handlers to use \
            a shared validate_email helper instead of duplicating the regex check",
        bug_cases: vec![
            BugInjectionCase {
                id: TaskId::new(),
                description: "Duplicated regex in both handlers (not extracted)",
                provider_response:
                    "fn login(email: &str) { let re = Regex::new(...); /* ... */ }\n\
                    fn register(email: &str) { let re = Regex::new(...); /* ... */ }",
                bug_patterns: &["duplicat", "duplicate", "DRY", "copy-paste"],
                should_catch: true,
            },
            BugInjectionCase {
                id: TaskId::new(),
                description: "Inline magic number instead of named constant",
                provider_response: "if age < 18 { return Err(\"too young\"); }",
                bug_patterns: &["magic", "constant", "18"],
                should_catch: true,
            },
            BugInjectionCase {
                id: TaskId::new(),
                description: "Removed error handling in extracted helper",
                provider_response: "fn validate_email(email: &str) -> bool { \
                    email.contains('@') } // no error context",
                bug_patterns: &["error", "result", "no error"],
                should_catch: true,
            },
            BugInjectionCase {
                id: TaskId::new(),
                description: "Unchanged old code still present (dead code)",
                provider_response: "// old validate function kept for reference\n\
                    fn old_validate(raw: &str) -> bool { raw.len() > 5 }",
                bug_patterns: &["dead", "unused", "legacy"],
                should_catch: true,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Scenario C: Fix a bug with hidden cause
// ---------------------------------------------------------------------------
//
// The mock researcher identifies the root cause; the mock coder provides a fix
// that addresses the symptom but not the root cause.  The reviewer is
// expected to flag the shallow fix.

pub fn scenario_c_bugfix_hidden_cause() -> BenchmarkScenario {
    BenchmarkScenario {
        id: TaskId::new(),
        name: "bugfix-hidden-cause",
        description: "Fix a race condition in the session cache",
        agent_task_description: "Investigate and fix the intermittent \
            'session not found' errors under concurrent requests",
        bug_cases: vec![
            BugInjectionCase {
                id: TaskId::new(),
                description: "Fix symptom (add retry) instead of root cause (add locking)",
                provider_response: "fn get_session(id: &str) -> Option<Session> {\n    \
                    for _ in 0..3 { if let Ok(s) = try_get(id) { return Some(s); } }\n    \
                    None\n}",
                bug_patterns: &["retry", "race", "lock", "mutex"],
                should_catch: true,
            },
            BugInjectionCase {
                id: TaskId::new(),
                description: "Increased timeout instead of fixing the data race",
                provider_response: "// increase timeout from 100ms to 500ms\n\
                    timeout_ms = 500;",
                bug_patterns: &["timeout", "race", "root cause"],
                should_catch: true,
            },
            BugInjectionCase {
                id: TaskId::new(),
                description: "Added logging but no actual fix",
                provider_response: "eprintln!(\"session lookup failed, retrying...\");\n\
                    // no actual fix applied",
                bug_patterns: &["log", "no fix", "cosmetic"],
                should_catch: true,
            },
        ],
    }
}

/// Return all three standard benchmark scenarios.
pub fn all_scenarios() -> Vec<BenchmarkScenario> {
    vec![scenario_a_add_feature(), scenario_b_refactor_module(), scenario_c_bugfix_hidden_cause()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_a_has_four_bug_cases() {
        let s = scenario_a_add_feature();
        assert_eq!(s.bug_cases.len(), 4);
        assert!(s.bug_cases.iter().all(|c| c.should_catch));
    }

    #[test]
    fn scenario_b_has_four_bug_cases() {
        let s = scenario_b_refactor_module();
        assert_eq!(s.bug_cases.len(), 4);
        assert!(s.bug_cases.iter().all(|c| c.should_catch));
    }

    #[test]
    fn scenario_c_has_three_bug_cases() {
        let s = scenario_c_bugfix_hidden_cause();
        assert_eq!(s.bug_cases.len(), 3);
        assert!(s.bug_cases.iter().all(|c| c.should_catch));
    }

    #[test]
    fn all_scenarios_returns_three() {
        let scenarios = all_scenarios();
        assert_eq!(scenarios.len(), 3);
    }

    #[test]
    fn each_scenario_has_unique_id() {
        let scenarios = all_scenarios();
        for i in 0..scenarios.len() {
            for j in i + 1..scenarios.len() {
                assert_ne!(scenarios[i].id, scenarios[j].id);
            }
        }
    }

    #[test]
    fn each_bug_case_has_non_empty_patterns() {
        for scenario in all_scenarios() {
            for case in &scenario.bug_cases {
                assert!(
                    !case.bug_patterns.is_empty(),
                    "case '{}' has no patterns",
                    case.description
                );
            }
        }
    }

    #[test]
    fn each_scenario_has_unique_name() {
        let scenarios = all_scenarios();
        let names: Vec<&str> = scenarios.iter().map(|s| s.name).collect();
        let mut unique_names = names.clone();
        unique_names.sort();
        unique_names.dedup();
        assert_eq!(names.len(), unique_names.len(), "scenario names must be unique");
    }

    #[test]
    fn all_bug_cases_have_non_empty_descriptions() {
        for scenario in all_scenarios() {
            for case in &scenario.bug_cases {
                assert!(
                    !case.description.is_empty(),
                    "scenario '{}' has a bug case with empty description",
                    scenario.name
                );
            }
        }
    }

    #[test]
    fn each_scenario_bug_cases_have_unique_ids() {
        for scenario in all_scenarios() {
            let mut seen = std::collections::HashSet::new();
            for case in &scenario.bug_cases {
                assert!(
                    seen.insert(case.id),
                    "scenario '{}' has duplicate bug case ID for '{}'",
                    scenario.name,
                    case.description
                );
            }
        }
    }
}
