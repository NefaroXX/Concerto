//! Benchmark for SimplePolicyEngine evaluation throughput with 100+ rules.
//!
//! Measures first-rule-match, last-rule-match, and no-match scenarios
//! to assess the cost of rule iteration and condition evaluation.

use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::policy::{AuditEntry, AuditLog, PolicyEngine};
use concerto_core::types::{CapabilitySet, Condition, PolicyAction, PolicyRule};
use concerto_core::CancellationToken;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::sync::Arc;

/// A no-op audit log for benchmarks.
struct NullAudit;

#[async_trait::async_trait]
impl AuditLog for NullAudit {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::error::PolicyError> {
        Ok(())
    }
}

/// Build a policy engine with `count` rules that all use `Always` conditions.
/// The last rule has a specific ToolName condition so we can test last-match.
fn build_engine(count: usize, match_at_end: bool) -> SimplePolicyEngine {
    let mut rules: Vec<PolicyRule> = (0..count - 1)
        .map(|i| {
            if match_at_end && i == count - 2 {
                // Second-to-last: the one we'll match
                PolicyRule::AutoApprove(Condition::ToolName("write_file".into()))
            } else {
                PolicyRule::AutoDeny(Condition::All(vec![
                    Condition::ToolName("noop".into()),
                    Condition::Always,
                ]))
            }
        })
        .collect();

    // Final rule: always-allow catchall (only hit if nothing else matched)
    if match_at_end {
        rules.push(PolicyRule::AutoApprove(Condition::Always));
    } else {
        rules.push(PolicyRule::AutoDeny(Condition::Always));
    }

    let audit = Arc::new(NullAudit);
    SimplePolicyEngine::new(rules, audit)
}

fn make_action<'a>(tool_name: &'a str, input: &'a serde_json::Value) -> PolicyAction<'a> {
    PolicyAction {
        tool_name,
        input,
        session_id: concerto_core::ids::Ulid::new(),
        correlation_id: concerto_core::ids::Ulid::new(),
        capability_requirements: CapabilitySet::default(),
        sandbox_profile: None,
        estimated_cost_usd: None,
        command_facts: None,
    }
}

fn bench_policy_evaluate(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cancel = CancellationToken::new();

    let empty_input = serde_json::Value::Object(Default::default());
    let glob_input = serde_json::json!({"path": "src/lib.rs"});

    // --- 100 rules, first match ---
    {
        let engine = build_engine(100, false);
        let action = make_action("write_file", &empty_input);
        c.bench_function("policy/100_rules/first_match", |b| {
            b.to_async(&rt).iter(|| engine.evaluate(black_box(&action), cancel.clone()));
        });
    }

    // --- 100 rules, no match (hit catchall) ---
    {
        let engine = build_engine(100, false);
        let action = make_action("unknown_tool", &empty_input);
        c.bench_function("policy/100_rules/no_match_catchall", |b| {
            b.to_async(&rt).iter(|| engine.evaluate(black_box(&action), cancel.clone()));
        });
    }

    // --- 500 rules, last match ---
    {
        let engine = build_engine(500, true);
        let action = make_action("write_file", &empty_input);
        c.bench_function("policy/500_rules/last_match", |b| {
            b.to_async(&rt).iter(|| engine.evaluate(black_box(&action), cancel.clone()));
        });
    }

    // --- 1000 rules, no match ---
    {
        let engine = build_engine(1000, false);
        let action = make_action("unknown_tool", &empty_input);
        c.bench_function("policy/1000_rules/no_match", |b| {
            b.to_async(&rt).iter(|| engine.evaluate(black_box(&action), cancel.clone()));
        });
    }

    // --- Compound condition (PathGlob regex match) ---
    {
        let mut rules: Vec<PolicyRule> =
            (0..99).map(|_| PolicyRule::AutoDeny(Condition::ToolName("noop".into()))).collect();
        rules.push(PolicyRule::AutoApprove(Condition::PathGlob("src/**/*.rs".into())));
        let engine = SimplePolicyEngine::new(rules, Arc::new(NullAudit));

        let action = PolicyAction {
            tool_name: "write_file",
            input: &glob_input,
            session_id: concerto_core::ids::Ulid::new(),
            correlation_id: concerto_core::ids::Ulid::new(),
            capability_requirements: CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        };

        c.bench_function("policy/100_rules/glob_match", |b| {
            b.to_async(&rt).iter(|| engine.evaluate(black_box(&action), cancel.clone()));
        });
    }
}

criterion_group!(benches, bench_policy_evaluate);
criterion_main!(benches);
