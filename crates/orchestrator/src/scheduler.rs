//! Critical-path-aware scheduling of the ready task batch (issue #58).
//!
//! A pure, deterministic scheduler: for every pending task whose graph
//! dependencies are all completed, it computes a priority score from live
//! [`TaskGraph`] structure plus small per-pick context (prior attempt
//! counts, agent-role availability, starvation aging), then orders the
//! ready batch, highest score first.
//!
//! # Signals and weights
//!
//! The score is an integer so ordering is exact (no float ambiguity).
//! Weights are documented constants; change them only with a rationale
//! commit.
//!
//! | Signal | Weight (per unit) | Rationale |
//! |---|---|---|
//! | Longest unfinished downstream chain (`CHAIN_W`) | +10 | Critical-path spine: finishing this task advances the longest remaining chain of work. |
//! | Unfinished direct dependents (`DEPENDENT_W`) | +5 | High fan-in work opens the DAG widest (parallelism). |
//! | Dependent whose ONLY unfinished dependency is this task (`UNLOCK_W`) | +20 | Sole-blocker awareness: keeps the head of an otherwise-blocked branch moving. |
//! | Prior failed (re-)attempts (`FAILURE_PENALTY_W`, bounded to `MAX_PENALIZED_ATTEMPTS`) | −5 per extra attempt | Failure risk: work the same conditions keep failing is deprioritized so other ready work proceeds — but the penalty is bounded, and the aging bonus below overrides it, so retries are never starved. |
//! | Preferred role with no registered agent (`ROLE_UNAVAILABLE_W`) | −100 | Availability fit: work whose specialist is missing yields the batch slot to dispatchable work. |
//! | Starvation aging (`AGING_W`, capped at `MAX_AGING_CYCLES`) | +40 per aged cycle | Aging bonus: a ready-but-undispatched task grows its score so independent (non-blocking) work still dispatches when the critical path is stuck. |
//!
//! # Determinism
//!
//! No model calls, no wall clock, no randomness in the score. Ties break
//! by ascending `TaskId` (a `Ulid`, totally ordered). Feeding the same
//! graph and context twice yields the same order.
//!
//! # Transform correctness (issue #57)
//!
//! Priorities derive from live graph state every cycle: the coordinator
//! calls [`schedule_batch`] freshly for each ready batch, so after a
//! split/merge rewrite moves nodes and edges, the next recomputation
//! reflects the new structure — no cached priority survives a transform.
//!
//! # Starvation guard
//!
//! `dispatch_limit` (an optional cap on picks per batch) plus the aging
//! map implement the yield rule: the caller increments the aging counter
//! of every ready-but-undispatched task each cycle. After
//! `MAX_AGING_CYCLES` cycles the aging bonus (`AGING_W * K`) is large
//! enough to outrank a one-dependent critical task
//! (`CHAIN_W + DEPENDENT_W + UNLOCK_W = 35` vs the capped `AGING_W * 3 =
//! 120`). Callers that dispatch the entire ready batch (no cap) reset the
//! aging counter of dispatched tasks; the guard then never engages.

use std::collections::{HashMap, HashSet};

use concerto_core::types::{AgentId, SubTask, SubTaskStatus, TaskId};

use crate::graph::TaskGraph;

/// Score weight per unfinished task on the longest downstream chain.
pub const CHAIN_W: i64 = 10;
/// Score weight per unfinished direct dependent.
pub const DEPENDENT_W: i64 = 5;
/// Score weight per dependent that ONLY this task is holding back.
pub const UNLOCK_W: i64 = 20;
/// Score penalty per prior failed attempt beyond the first dispatch.
pub const FAILURE_PENALTY_W: i64 = 5;
/// Re-attempts that still accrue penalty (bounded — retries deprioritized,
/// never annihilated).
pub const MAX_PENALIZED_ATTEMPTS: u32 = 3;
/// Score penalty when the preferred agent role has no registered agent.
pub const ROLE_UNAVAILABLE_W: i64 = 100;
/// Score bonus per starvation-aging cycle.
pub const AGING_W: i64 = 40;
/// Aging cycles beyond which the bonus stops growing (bounded guard).
pub const MAX_AGING_CYCLES: u32 = 3;

/// Inputs the pure scheduler consumes in addition to graph structure.
///
/// All fields are borrowed references; the coordinator assembles one
/// context per ready-batch cycle. The scheduler performs NO I/O and NO
/// model calls.
pub struct SchedulerContext<'a> {
    /// Prior dispatch attempts per task (`subtask_attempts`). The failure
    /// signal: attempts > 1 means earlier dispatches of this task failed.
    pub attempts: &'a HashMap<TaskId, u32>,
    /// Roles whose agent is currently registered; `None` disables the
    /// availability-fit signal (pure tests and registry-less callers).
    pub available_roles: Option<&'a HashSet<AgentId>>,
    /// Starvation-aging counters: for a ready-but-not-dispatched task,
    /// how many prior batch cycles it waited without being dispatched.
    pub aging: &'a HashMap<TaskId, u32>,
}

/// One ranked pick: the task, its score, and WHY it ranked where it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledPick {
    pub task_id: TaskId,
    pub score: i64,
    /// Deterministic reason rendered from the same signals as the score,
    /// e.g. "unlocks 2 downstream (critical chain of 2)" or
    /// "sole blocker for <task-id>; recoverable retry #3 (deprioritized)".
    pub reason: String,
}

/// Structure-only facts for one task, derived from the live graph.
#[derive(Debug)]
struct DownstreamStructure {
    /// Length of the longest chain of unfinished tasks downstream of this
    /// one (excluding the task itself).
    chain_len: usize,
    /// Unfinished direct dependents.
    direct_dependents: usize,
    /// Unfinished dependent ids whose ONLY unfinished dependency is this
    /// task — they become ready the moment it completes. Sorted ascending,
    /// so rendering stays deterministic.
    solely_blocked: Vec<TaskId>,
}

/// Deterministic priority score for one pending task.
///
/// Recovery signals read from the graph (`SubTaskStatus`) and the context
/// (attempt counts, aging, role availability) combine additively; every
/// weight is a documented module constant.
pub fn priority_score(task: &SubTask, graph: &TaskGraph, context: &SchedulerContext<'_>) -> i64 {
    let structure = downstream_structure(task, graph);
    let mut score = 0i64;
    score += CHAIN_W * structure.chain_len as i64;
    score += DEPENDENT_W * structure.direct_dependents as i64;
    score += UNLOCK_W * structure.solely_blocked.len() as i64;

    // Failure-risk adjustment: the FIRST attempt (a fresh task) is never
    // penalized; each earlier failed round costs a bounded chunk.
    let attempts = context.attempts.get(&task.id).copied().unwrap_or(0);
    if attempts > 1 {
        let penalized = attempts.saturating_sub(1).min(MAX_PENALIZED_ATTEMPTS);
        score -= FAILURE_PENALTY_W * penalized as i64;
    }

    // Availability fit.
    if let Some(available) = context.available_roles {
        if !available.contains(&task.role) {
            score -= ROLE_UNAVAILABLE_W;
        }
    }

    // Starvation-aging bonus (capped).
    let aged = context.aging.get(&task.id).copied().unwrap_or(0);
    score += AGING_W * aged.min(MAX_AGING_CYCLES) as i64;

    score
}

/// Rank the current ready batch deterministically.
///
/// Picks come back ordered score-descending, then `TaskId` ascending as
/// the tie-break. When `dispatch_limit` is `Some(n)`, only the first `n`
/// picks are returned — the caller must then increment the aging counter
/// of every ready task that was not picked (the starvation yield
/// protocol). A `None` limit returns the full ranked batch.
pub fn schedule_batch(
    graph: &TaskGraph,
    context: &SchedulerContext<'_>,
    dispatch_limit: Option<usize>,
) -> Vec<ScheduledPick> {
    let mut ranked: Vec<(i64, TaskId, SubTask)> = graph
        .ready_tasks()
        .into_iter()
        .map(|task| (priority_score(task, graph, context), task.id, task.clone()))
        .collect();
    // (score desc, id asc) is a total, deterministic order regardless of
    // the underlying HashMap iteration order of `ready_tasks()`.
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1 .0.cmp(&b.1 .0)));

    let take = dispatch_limit.unwrap_or(usize::MAX);
    ranked
        .into_iter()
        .take(take)
        .map(|(score, id, task)| ScheduledPick {
            task_id: id,
            score,
            reason: explain(&task, graph, context),
        })
        .collect()
}

fn downstream_structure(task: &SubTask, graph: &TaskGraph) -> DownstreamStructure {
    let mut direct = 0usize;
    let mut solely: Vec<TaskId> = Vec::new();
    for (dependent, _dependency) in graph.outgoing_dependents(&task.id) {
        if graph.get(&dependent).is_some_and(|d| d.status == SubTaskStatus::Completed) {
            continue;
        }
        direct += 1;
        // Sole-blocker check: every OTHER dependency of `dependent` is
        // already completed; only this task holds it back.
        let other_unfinished = graph
            .dependencies_of(&dependent)
            .into_iter()
            .filter(|dep| *dep != task.id)
            .any(|dep| graph.get(&dep).is_some_and(|d| d.status != SubTaskStatus::Completed));
        if !other_unfinished {
            solely.push(dependent);
        }
    }
    solely.sort_unstable_by_key(|id| id.0);

    DownstreamStructure {
        chain_len: longest_downstream_unfinished_chain(&task.id, graph),
        direct_dependents: direct,
        solely_blocked: solely,
    }
}

/// Longest downstream chain of unfinished tasks, computed by relaxation.
///
/// Bellman-Ford-style iterate-until-stable over a DAG: at most one pass
/// per graph node and each pass is bounded by nodes + edges — no
/// recursion, bounded loops. A completed node counts 0 itself but still
/// relays chain length from beyond it.
fn longest_downstream_unfinished_chain(from: &TaskId, graph: &TaskGraph) -> usize {
    if graph.is_empty() {
        return 0;
    }
    // Value of a node = longest unfinished chain strictly downstream of it.
    let mut value: HashMap<TaskId, usize> = HashMap::with_capacity(graph.len());
    for _pass in 0..graph.len() {
        let mut changed = false;
        for reach in graph.all_tasks() {
            let mut best = 0usize;
            for (dependent, _dependency) in graph.outgoing_dependents(&reach.id) {
                let dep_here = if graph
                    .get(&dependent)
                    .is_some_and(|d| d.status == SubTaskStatus::Completed)
                {
                    0
                } else {
                    1
                };
                let beyond = value.get(&dependent).copied().unwrap_or(0);
                best = best.max(dep_here + beyond);
            }
            if best != value.get(&reach.id).copied().unwrap_or(0) {
                value.insert(reach.id, best);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut best = 0usize;
    for (dependent, _dependency) in graph.outgoing_dependents(from) {
        let dep_here =
            if graph.get(&dependent).is_some_and(|d| d.status == SubTaskStatus::Completed) {
                0
            } else {
                1
            };
        let beyond = value.get(&dependent).copied().unwrap_or(0);
        best = best.max(dep_here + beyond);
    }
    best
}

/// Render the WHY for one pick, deterministically composed from the same
/// signals as the score.
fn explain(task: &SubTask, graph: &TaskGraph, context: &SchedulerContext<'_>) -> String {
    let structure = downstream_structure(task, graph);
    let attempts = context.attempts.get(&task.id).copied().unwrap_or(0);
    let mut parts: Vec<String> = Vec::new();

    parts.push(format!("unlocks {} downstream", structure.direct_dependents));
    if structure.chain_len > 0 {
        parts.push(format!("critical chain of {}", structure.chain_len));
    }
    if structure.direct_dependents == 0 {
        parts.push("no downstream dependents".to_owned());
    }
    if !structure.solely_blocked.is_empty() {
        let listed: Vec<String> =
            structure.solely_blocked.iter().take(3).map(|id| id.to_string()).collect();
        let more = if structure.solely_blocked.len() > 3 { "…" } else { "" };
        parts.push(format!("sole blocker for {}{}", listed.join(", "), more));
    }

    if attempts > 1 {
        parts.push("recoverable retry #".to_owned() + &attempts.to_string() + " (deprioritized)");
    }
    if let Some(available) = context.available_roles {
        if !available.contains(&task.role) {
            parts.push(format!("preferred role {} unavailable", task.role.as_str()));
        }
    }
    let aged = context.aging.get(&task.id).copied().unwrap_or(0);
    if aged > 0 {
        parts.push(format!("aged {} cycle(s) ready without dispatch", aged));
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;

    use crate::graph::Dependency;
    use crate::relationship::AgentRelationship;

    fn subtask(role: &str, description: &str) -> SubTask {
        SubTask::new(Ulid::new(), AgentId::new(role), description)
    }

    fn ctx<'a>(
        attempts: &'a HashMap<TaskId, u32>,
        aging: &'a HashMap<TaskId, u32>,
    ) -> SchedulerContext<'a> {
        SchedulerContext { attempts, available_roles: None, aging }
    }

    /// Diamond: A completed, B and C pending children of A, D pending on
    /// both B and C; E is an independent root. Both diamond legs outrank
    /// the isolated task, with the documented reasons.
    #[test]
    fn diamond_picks_critical_leg_over_isolated_work_with_reasons() {
        let mut graph = TaskGraph::new();
        let a = subtask("architect", "root");
        let b = subtask("coder", "leg one");
        let c = subtask("coder", "leg two");
        let d = subtask("reviewer", "join");
        let e = subtask("coder", "side quest");
        graph.add_root(a.clone());
        graph.add_child(b.clone(), a.id, Dependency::MustFinishBefore);
        graph.add_child(c.clone(), a.id, Dependency::MustFinishBefore);
        graph.add_child_with_relationship(
            d.clone(),
            b.id,
            Dependency::MustFinishBefore,
            AgentRelationship::ProvidesContextTo,
        );
        graph.add_dependency(d.id, c.id, Dependency::MustFinishBefore).unwrap();
        graph.add_root(e.clone());
        graph.mark_done(&a.id);

        let attempts = HashMap::new();
        let aging = HashMap::new();
        let picks = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        assert_eq!(picks.len(), 3, "B, C, E are ready");
        let pos_of = |id: &TaskId| picks.iter().position(|p| &p.task_id == id).unwrap();
        assert!(pos_of(&b.id) < pos_of(&e.id));
        assert!(pos_of(&c.id) < pos_of(&e.id));
        for pick in &picks {
            if pick.task_id == b.id || pick.task_id == c.id {
                assert!(
                    pick.reason.contains("unlocks 1 downstream")
                        && pick.reason.contains("critical chain of 1"),
                    "{}",
                    pick.reason
                );
            } else {
                assert!(pick.reason.contains("no downstream dependents"), "{}", pick.reason);
            }
        }
    }

    /// A ready task whose only downstream dependent has no OTHER unfinished
    /// dependency outranks an equally isolated peer, with a sole-blocker
    /// reason; the held-back dependent stays out of the batch.
    #[test]
    fn sole_blocker_outranks_isolated_work() {
        let mut graph = TaskGraph::new();
        let a = subtask("architect", "gateway");
        let head = subtask("coder", "gate head");
        let behind = subtask("reviewer", "held back");
        let independent = subtask("tester", "independent");
        graph.add_root(a.clone());
        graph.add_child(head.clone(), a.id, Dependency::MustFinishBefore);
        graph.add_child(behind.clone(), head.id, Dependency::MustFinishBefore);
        graph.add_root(independent.clone());
        graph.mark_done(&a.id);

        let attempts = HashMap::new();
        let aging = HashMap::new();
        let picks = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        assert_eq!(picks.len(), 2, "head + independent; behind stays blocked");
        assert_eq!(picks[0].task_id, head.id);
        assert!(picks[0].reason.contains("sole blocker for"), "{}", picks[0].reason);
        assert_eq!(picks[1].task_id, independent.id);
    }

    /// A longer unfinished downstream chain ranks ahead of a shorter one.
    #[test]
    fn longer_critical_chain_outranks_shorter() {
        let mut graph = TaskGraph::new();
        let a = subtask("architect", "root");
        graph.add_root(a.clone());
        let short = subtask("coder", "short leg");
        let short_tail = subtask("tester", "short tail");
        graph.add_child(short.clone(), a.id, Dependency::MustFinishBefore);
        graph.add_child(short_tail.clone(), short.id, Dependency::MustFinishBefore);
        let long = subtask("coder", "long leg");
        let long_mid = subtask("tester", "long mid");
        let long_tail = subtask("reviewer", "long tail");
        graph.add_child(long.clone(), a.id, Dependency::MustFinishBefore);
        graph.add_child(long_mid.clone(), long.id, Dependency::MustFinishBefore);
        graph.add_child(long_tail.clone(), long_mid.id, Dependency::MustFinishBefore);
        graph.mark_done(&a.id);

        let attempts = HashMap::new();
        let aging = HashMap::new();
        let picks = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        assert_eq!(picks[0].task_id, long.id, "longer chain first: {:?}", picks);
        assert_eq!(picks[1].task_id, short.id);
    }

    /// Prior failures deprioritize a retry against identical fresh work but
    /// keep the retry IN the batch (bounded penalty, no starvation), and
    /// enough aging cycles override the bounded penalty.
    #[test]
    fn failure_penalty_adjusts_priority_without_starvation() {
        let mut graph = TaskGraph::new();
        let a = subtask("architect", "root");
        graph.add_root(a.clone());
        let flaky = subtask("coder", "recurring failure");
        graph.add_child(flaky.clone(), a.id, Dependency::MustFinishBefore);
        let fresh = subtask("coder", "fresh job");
        graph.add_child(fresh.clone(), a.id, Dependency::MustFinishBefore);
        graph.mark_done(&a.id);

        let mut attempts = HashMap::new();
        attempts.insert(flaky.id, 5u32);

        let aging = HashMap::new();
        let picks = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        let pos_of = |id: &TaskId| picks.iter().position(|p| p.task_id == *id).unwrap();
        assert!(pos_of(&flaky.id) > pos_of(&fresh.id), "failed retry loses to fresh");
        assert!(
            picks[pos_of(&flaky.id)].reason.contains("recoverable retry"),
            "why is exposed: {}",
            picks[pos_of(&flaky.id)].reason
        );
        // The penalty is BOUNDED: retry ranks last, not dead.
        assert_eq!(picks.last().unwrap().task_id, flaky.id);

        // Aging override: the bounded penalty is beaten by the capped aging
        // bonus (5*3 penalty < 40*3 bonus).
        let mut aging_now = HashMap::new();
        aging_now.insert(flaky.id, 3u32);
        let picks = schedule_batch(&graph, &ctx(&attempts, &aging_now), None);
        assert_eq!(picks[0].task_id, flaky.id, "aged retry outranks everything");
    }

    /// A pending chain whose head is BLOCKED leaves independent ready work
    /// runnable immediately — the blocked critical path cannot starve it.
    #[test]
    fn blocked_critical_path_yields_to_independent_work() {
        let mut graph = TaskGraph::new();
        let mut head = subtask("reviewer", "critical head");
        head.status = SubTaskStatus::Blocked;
        let tail = subtask("coder", "critical tail");
        graph.add_root(head.clone());
        graph.add_child(tail.clone(), head.id, Dependency::MustFinishBefore);
        let independent = subtask("coder", "independent");
        graph.add_root(independent.clone());

        let attempts = HashMap::new();
        let aging = HashMap::new();
        let picks = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        assert_eq!(picks.len(), 1, "only the independent task is ready");
        assert_eq!(picks[0].task_id, independent.id);
        assert!(picks[0].reason.contains("no downstream dependents"));
    }

    /// With a dispatch limit, ready-but-undispatched aging flips the pick
    /// after one cycle, and the bonus is CAPPED at `MAX_AGING_CYCLES`.
    #[test]
    fn aging_bonus_yields_and_is_bounded() {
        let mut graph = TaskGraph::new();
        let a = subtask("architect", "root");
        graph.add_root(a.clone());
        let critical = subtask("coder", "critical leg");
        graph.add_child(critical.clone(), a.id, Dependency::MustFinishBefore);
        // A dependent under `critical` makes it genuinely critical (chain of
        // 1), so the fresh-state pick below is structural, not a tie.
        let critical_tail = subtask("tester", "critical tail");
        graph.add_child(critical_tail.clone(), critical.id, Dependency::MustFinishBefore);
        let low = subtask("coder", "low priority");
        graph.add_child(low.clone(), a.id, Dependency::MustFinishBefore);
        graph.mark_done(&a.id);

        let attempts = HashMap::new();
        let no_aging = HashMap::new();

        // Fresh state: the critical task wins the pick.
        let picks = schedule_batch(&graph, &ctx(&attempts, &no_aging), Some(1));
        assert_eq!(picks[0].task_id, critical.id);

        // One aged cycle flips the yield: the ready-but-waiting task
        // outranks the critical one.
        let mut aging = HashMap::new();
        aging.insert(low.id, 1u32);
        let picks = schedule_batch(&graph, &ctx(&attempts, &aging), Some(1));
        assert_eq!(picks[0].task_id, low.id);

        // The bonus stops growing at MAX_AGING_CYCLES: ten aged cycles
        // score exactly the same as three.
        aging.insert(low.id, 10u32);
        let capped = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        assert_eq!(capped[0].score, AGING_W * MAX_AGING_CYCLES as i64, "aging bonus capped");
    }

    /// Same graph + same inputs → identical ranked output twice; a rebuilt
    /// graph with identical statuses ties to the same ordering.
    #[test]
    fn duplicate_schedules_are_identical() {
        let mut graph = TaskGraph::new();
        let a = subtask("architect", "root");
        graph.add_root(a.clone());
        let b = subtask("coder", "one");
        let c = subtask("reviewer", "two");
        let d = subtask("coder", "three");
        graph.add_child(b.clone(), a.id, Dependency::MustFinishBefore);
        graph.add_child(c.clone(), a.id, Dependency::MustFinishBefore);
        graph.add_child(d.clone(), b.id, Dependency::MustFinishBefore);
        graph.mark_done(&a.id);

        let attempts = HashMap::new();
        let aging = HashMap::new();
        let one = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        let two = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        assert_eq!(one, two, "the same graph must schedule identically twice");

        // Scores depend on structure only, so a graph rebuilt from scratch
        // with the same statuses and edges produces the same ORDER.
        let mut rebuilt = TaskGraph::new();
        for task in graph.all_tasks() {
            rebuilt.add_root(task.clone());
        }
        rebuilt.add_dependency(b.id, a.id, Dependency::MustFinishBefore).unwrap();
        rebuilt.add_dependency(c.id, a.id, Dependency::MustFinishBefore).unwrap();
        rebuilt.add_dependency(d.id, b.id, Dependency::MustFinishBefore).unwrap();
        let three = schedule_batch(&rebuilt, &ctx(&attempts, &aging), None);
        assert_eq!(
            three.iter().map(|p| p.score).collect::<Vec<_>>(),
            two.iter().map(|p| p.score).collect::<Vec<_>>()
        );
    }

    /// Issue-#57 transforms: SPLIT a critical task mid-run, re-prioritize,
    /// MERGE back, re-prioritize — ordering always reflects the live graph.
    #[test]
    fn split_and_merge_transforms_recompute_priorities() {
        use crate::task_transform::{apply_transform, SplitChildSpec, TaskTransformSpec};

        let mut graph = TaskGraph::new();
        let a = subtask("architect", "root");
        graph.add_root(a.clone());
        let busy = subtask("coder", "busy leg");
        graph.add_child(busy.clone(), a.id, Dependency::MustFinishBefore);
        let final_task = subtask("reviewer", "final gate");
        // busy → final_task gives `busy` a real unfinished downstream chain
        // to rank on BEFORE the split.
        graph.add_child(final_task.clone(), busy.id, Dependency::MustFinishBefore);
        let independent = subtask("coder", "independent");
        graph.add_root(independent.clone());
        graph.mark_done(&a.id);

        let attempts = HashMap::new();
        let aging = HashMap::new();

        // BEFORE the split, `busy` leads on the chain through `final_task`.
        let before = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        assert_eq!(before[0].task_id, busy.id);
        assert!(before[0].reason.contains("critical chain of 1"), "{}", before[0].reason);

        let spec = TaskTransformSpec::Split {
            parent: busy.id,
            children: vec![
                SplitChildSpec {
                    description: "part one".into(),
                    expected_artifacts: Vec::new(),
                    after: Vec::new(),
                },
                SplitChildSpec {
                    description: "part two".into(),
                    expected_artifacts: Vec::new(),
                    after: vec![0],
                },
            ],
        };
        let now = time::OffsetDateTime::now_utc();
        let outcome = apply_transform(&mut graph, &spec, now).expect("legal split applies");
        let crate::task_transform::TransformOutcome::Split(split) = &outcome else {
            panic!("expected split outcome");
        };
        let [child1, child2] = &split.children[..] else {
            panic!("expected exactly two split children");
        };
        // The parent is gone from the live graph.
        assert!(graph.get(&busy.id).is_none());

        // AFTER the split: the first child is ready and its priority is
        // recomputed from the NEW edges. Its chain now runs through the
        // second child to `final_task` (chain of 2, up from 1), and it is
        // the sole blocker for the second child.
        let after = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        let pos_of = |id: &TaskId| after.iter().position(|p| p.task_id == *id).unwrap();
        assert!(pos_of(child1) < pos_of(&independent.id));
        assert!(after[0].reason.contains("critical chain of 2"), "{}", after[0].reason);
        assert!(after[0].reason.contains("sole blocker for"), "{}", after[0].reason);

        // NOW MERGE the two children back into one survivor.
        let merge_spec = TaskTransformSpec::Merge {
            task_ids: vec![*child1, *child2],
            merged_description: "re-fused work".into(),
            merged_artifacts: Vec::new(),
        };
        let merged = apply_transform(&mut graph, &merge_spec, now).expect("legal merge applies");
        let crate::task_transform::TransformOutcome::Merge(merge) = &merged else {
            panic!("expected merge outcome");
        };
        let survivor = merge.survivor;

        // AFTER the merge the survivor's priority again reflects live
        // state (the previous child-chain reasoning is gone; the survivor
        // is ready and outranks nothing-but-nothing by its sole chain).
        let final_picks = schedule_batch(&graph, &ctx(&attempts, &aging), None);
        assert!(final_picks.iter().position(|p| p.task_id == survivor).is_some());
    }
}
