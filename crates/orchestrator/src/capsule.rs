//! ADR-64 §6: Task-specific workspace capsule — file metadata from the timeline
//! preloaded into agent prompts so agents never re-read files merely to confirm
//! existence.
//!
//! The capsule is a **pure derived view** over the in-memory [`TimelineProjection`]
//! and `TaskGraph`. It contains no filesystem I/O — only metadata already
//! materialized by the projection builder (Phase 2) and the graph.
//!
//! ## Budget constants (ADR-64 §6 table)
//!
//! | Constant | Value | Rationale |
//! |----------|-------|-----------|
//! | `MAX_TIMELINE_ENTRIES` | 30 | Matches `MAX_TASKS` (24) + planning overhead |
//! | `MAX_ACTIVE_FILES` | 12 | Matches `MAX_PREVIOUS_RESULTS` (8) × 1.5 |
//! | `MAX_ENTRY_CHARS` | 400 | Half of `MAX_DETAIL_CHARS` (800) for balance |
//! | `MAX_OBSERVATION_SEQ` | 200 | Cap on served observation sequence length |

use concerto_core::types::{
    CapsuleFileEntry, CapsulePendingTask, SubTaskStatus, TaskId, WorkspaceCapsule,
};

use crate::graph::TaskGraph;
use crate::timeline::TimelineEvent;

// ---------------------------------------------------------------------------
// Budget constants (ADR-64 §6)
// ---------------------------------------------------------------------------

/// Maximum timeline entries to include in the capsule.
pub(crate) const MAX_TIMELINE_ENTRIES: usize = 30;

/// Maximum active/modified files to include in the capsule.
pub(crate) const MAX_ACTIVE_FILES: usize = 12;

/// Maximum characters per entry description.
pub(crate) const MAX_ENTRY_CHARS: usize = 400;

/// Maximum observation sequence length served in the capsule.
/// Reserved for Phase 7 observation sequences; currently unused.
#[allow(dead_code)]
pub(crate) const MAX_OBSERVATION_SEQ: usize = 200;

/// Total character budget for the formatted capsule block.
pub(crate) const MAX_CAPSULE_CHARS: usize = 4_000;

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Build a workspace capsule for `task_id` from the timeline projection and
/// task graph. Pure: no filesystem I/O, no LLM calls.
///
/// - `projection`: the timeline projection built at dispatch-batch boundary.
/// - `task_id`: the task about to be dispatched.
/// - `graph`: the current execution graph (for pending tasks).
/// - `expected_outputs`: the pre-computed artifact expectations for this task.
pub fn build_capsule(
    projection: &crate::timeline::TimelineProjection,
    task_id: &TaskId,
    graph: &TaskGraph,
    expected_outputs: &[camino::Utf8PathBuf],
) -> WorkspaceCapsule {
    // 1. Known files: walk WroteFile events, dedup by path, keep latest.
    let mut known_by_path: std::collections::HashMap<String, CapsuleFileEntry> =
        std::collections::HashMap::new();
    for event in &projection.events {
        if let TimelineEvent::WroteFile { gate_seq, path, content_hash, .. } = event {
            known_by_path.entry(path.clone()).or_insert_with(|| CapsuleFileEntry {
                path: path.clone(),
                content_hash: content_hash.clone(),
                last_modified_gate_seq: *gate_seq,
            });
        }
    }
    let mut known_files: Vec<CapsuleFileEntry> = known_by_path.into_values().collect();
    known_files.sort_by_key(|b| std::cmp::Reverse(b.last_modified_gate_seq));
    known_files.truncate(MAX_TIMELINE_ENTRIES);

    // 2. Modified files: walk completed results for tasks that are dependencies
    //    of `task_id`, collecting their `files_modified`.
    let deps = graph.dependencies_of(task_id);
    let mut modified_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut modified_files: Vec<CapsuleFileEntry> = Vec::new();
    for dep_id in &deps {
        if let Some(result) = projection.completed_results.get(dep_id) {
            for path in &result.files_modified {
                let path_str = path.to_string();
                if modified_set.insert(path_str.clone()) {
                    modified_files.push(CapsuleFileEntry {
                        path: path_str,
                        content_hash: String::new(), // Content hash not in result; use empty.
                        last_modified_gate_seq: 0,   // Not available from result alone.
                    });
                }
            }
        }
    }
    modified_files.truncate(MAX_ACTIVE_FILES);

    // 3. Pending work: tasks in the graph that are not yet completed and not
    //    the current task itself.
    let pending_work: Vec<CapsulePendingTask> = graph
        .all_tasks()
        .iter()
        .filter(|t| t.id != *task_id && t.status != SubTaskStatus::Completed)
        .take(MAX_TIMELINE_ENTRIES)
        .map(|t| {
            let mut desc = t.description.clone();
            desc.truncate(MAX_ENTRY_CHARS);
            CapsulePendingTask {
                task_id: t.id.to_string(),
                description: desc,
                dependencies: t.dependencies.iter().map(|d| d.to_string()).collect(),
            }
        })
        .collect();

    // 4. Expected outputs as strings.
    let expected_outputs: Vec<String> = expected_outputs.iter().map(|p| p.to_string()).collect();

    WorkspaceCapsule { known_files, modified_files, pending_work, expected_outputs }
}

// ---------------------------------------------------------------------------
// Formatter
// ---------------------------------------------------------------------------

/// Format a workspace capsule into a bounded XML block suitable for injection
/// into an agent prompt. The output is capped at [`MAX_CAPSULE_CHARS`].
pub fn format_capsule(capsule: &WorkspaceCapsule) -> String {
    if capsule.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(1024);
    out.push_str("<workspace_capsule>\n");
    out.push_str("Task-specific workspace context. File existence and hashes come from the timeline; do NOT re-read files merely to confirm they exist.\n");

    // Known files
    if !capsule.known_files.is_empty() {
        out.push_str("\n<known_files>\n");
        for entry in &capsule.known_files {
            let path = clip_str(&entry.path, MAX_ENTRY_CHARS);
            let hash = clip_str(&entry.content_hash, MAX_ENTRY_CHARS);
            out.push_str(&format!(
                "  <file path=\"{path}\" hash=\"{hash}\" gate_seq=\"{}\" />\n",
                entry.last_modified_gate_seq,
            ));
        }
        out.push_str("</known_files>\n");
    }

    // Modified files (from dependencies)
    if !capsule.modified_files.is_empty() {
        out.push_str("\n<modified_files>\n");
        for entry in &capsule.modified_files {
            let path = clip_str(&entry.path, MAX_ENTRY_CHARS);
            out.push_str(&format!("  <file path=\"{path}\" />\n"));
        }
        out.push_str("</modified_files>\n");
    }

    // Pending work
    if !capsule.pending_work.is_empty() {
        out.push_str("\n<pending_work>\n");
        for task in &capsule.pending_work {
            let id = clip_str(&task.task_id, MAX_ENTRY_CHARS);
            let desc = clip_str(&task.description, MAX_ENTRY_CHARS);
            out.push_str(&format!("  <task id=\"{id}\">{desc}</task>\n"));
        }
        out.push_str("</pending_work>\n");
    }

    // Expected outputs
    if !capsule.expected_outputs.is_empty() {
        out.push_str("\n<expected_outputs>\n");
        for path in &capsule.expected_outputs {
            let path = clip_str(path, MAX_ENTRY_CHARS);
            out.push_str(&format!("  <output path=\"{path}\" />\n"));
        }
        out.push_str("</expected_outputs>\n");
    }

    out.push_str("</workspace_capsule>");

    // Enforce total budget.
    clip_str(&out, MAX_CAPSULE_CHARS)
}

/// Clip a string to `max_chars`, appending `[clipped]` if truncated.
fn clip_str(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let clipped = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{clipped}[clipped]")
    } else {
        clipped
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::TaskGraph;
    use crate::timeline::{TimelineEvent, TimelineProjection};
    use camino::Utf8PathBuf;
    use concerto_core::ids::Ulid;
    use concerto_core::types::{
        AgentId, AgentOutcome, AgentRunResult, SubTask, SubTaskStatus, TaskId,
    };
    use std::collections::HashMap;

    fn make_task(id_str: &str, desc: &str, status: SubTaskStatus) -> SubTask {
        let id = TaskId(Ulid::from_string(id_str).expect("valid ULID string"));
        SubTask {
            id,
            parent_id: None,
            session_id: Ulid::new(),
            description: desc.to_owned(),
            role: AgentId::new("coder"),
            status,
            dependencies: Vec::new(),
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        }
    }

    fn task_id(id_str: &str) -> TaskId {
        TaskId(Ulid::from_string(id_str).expect("valid ULID string"))
    }

    fn empty_projection() -> TimelineProjection {
        TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        }
    }

    // ------------------------------------------------------------------
    // Test 1: empty capsule
    // ------------------------------------------------------------------

    #[test]
    fn empty_projection_yields_empty_capsule() {
        let projection = empty_projection();
        let mut graph = TaskGraph::new();
        let task = make_task("01HXYZ00000000000000000001", "test task", SubTaskStatus::Pending);
        let task_id = task.id;
        graph.add_root(task);

        let capsule = build_capsule(&projection, &task_id, &graph, &[]);
        assert!(capsule.known_files.is_empty());
        assert!(capsule.modified_files.is_empty());
        assert!(capsule.pending_work.is_empty());
        assert!(capsule.expected_outputs.is_empty());
        assert!(capsule.is_empty());
    }

    // ------------------------------------------------------------------
    // Test 2: known_files from WroteFile events, truncated at MAX_TIMELINE_ENTRIES
    // ------------------------------------------------------------------

    #[test]
    fn known_files_collected_from_wrote_file_events() {
        let mut projection = empty_projection();
        for i in 0..35 {
            projection.events.push(TimelineEvent::WroteFile {
                gate_seq: i as u64,
                path: format!("src/file_{i}.rs"),
                content_hash: format!("hash_{i}"),
                created_at: 1_700_000_000_000 + i as i64,
            });
        }

        let mut graph = TaskGraph::new();
        let task = make_task("01HXYZ00000000000000000002", "test task", SubTaskStatus::Pending);
        let task_id = task.id;
        graph.add_root(task);

        let capsule = build_capsule(&projection, &task_id, &graph, &[]);
        // Should be capped at MAX_TIMELINE_ENTRIES (30).
        assert_eq!(capsule.known_files.len(), MAX_TIMELINE_ENTRIES);
        // Most recent first (sorted by gate_seq desc).
        assert_eq!(capsule.known_files[0].path, "src/file_34.rs");
    }

    // ------------------------------------------------------------------
    // Test 3: modified_files from completed dependencies
    // ------------------------------------------------------------------

    #[test]
    fn modified_files_from_completed_dependencies() {
        let mut projection = empty_projection();

        let dep_id = task_id("01HXYZ00000000000000000010");
        let result_files = vec![
            Utf8PathBuf::from("src/a.rs"),
            Utf8PathBuf::from("src/b.rs"),
        ];
        projection.completed_results.insert(
            dep_id,
            AgentRunResult {
                task_id: dep_id,
                role: AgentId::new("coder"),
                outcome: AgentOutcome::Success,
                summary: "done".into(),
                files_modified: result_files,
                tool_call_count: 0,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: String::new(),
                model: String::new(),
                tokens_in: 0,
                tokens_out: 0,
            },
        );

        let mut graph = TaskGraph::new();
        let dep_task =
            make_task("01HXYZ00000000000000000010", "dep task", SubTaskStatus::Completed);
        graph.add_root(dep_task);

        let mut task =
            make_task("01HXYZ00000000000000000011", "current task", SubTaskStatus::Pending);
        let task_id = task.id;
        task.dependencies.push(dep_id);
        graph.add_child(task, dep_id, crate::graph::Dependency::MustFinishBefore);

        let capsule = build_capsule(&projection, &task_id, &graph, &[]);
        assert_eq!(capsule.modified_files.len(), 2);
        assert!(capsule.modified_files.iter().any(|f| f.path == "src/a.rs"));
        assert!(capsule.modified_files.iter().any(|f| f.path == "src/b.rs"));
    }

    // ------------------------------------------------------------------
    // Test 4: pending_work from graph
    // ------------------------------------------------------------------

    #[test]
    fn pending_work_from_graph() {
        let projection = empty_projection();
        let mut graph = TaskGraph::new();

        let t1 = make_task("01HXYZ00000000000000000020", "task one", SubTaskStatus::Pending);
        graph.add_root(t1);

        let t2 = make_task("01HXYZ00000000000000000021", "task two", SubTaskStatus::Pending);
        graph.add_child(
            t2,
            task_id("01HXYZ00000000000000000020"),
            crate::graph::Dependency::MustFinishBefore,
        );

        let tid = task_id("01HXYZ00000000000000000020");
        let capsule = build_capsule(&projection, &tid, &graph, &[]);

        // t2 is pending and not the current task; t1 is the current task so excluded.
        assert_eq!(capsule.pending_work.len(), 1);
        assert_eq!(capsule.pending_work[0].task_id, "01HXYZ00000000000000000021");
        assert_eq!(capsule.pending_work[0].description, "task two");
    }

    // ------------------------------------------------------------------
    // Test 5: expected_outputs forwarded
    // ------------------------------------------------------------------

    #[test]
    fn expected_outputs_forwarded() {
        let projection = empty_projection();
        let mut graph = TaskGraph::new();
        let task = make_task("01HXYZ00000000000000000030", "task", SubTaskStatus::Pending);
        let task_id = task.id;
        graph.add_root(task);

        let outputs =
            vec![Utf8PathBuf::from("dist/output_a.txt"), Utf8PathBuf::from("dist/output_b.txt")];
        let capsule = build_capsule(&projection, &task_id, &graph, &outputs);
        assert_eq!(capsule.expected_outputs.len(), 2);
        assert!(capsule.expected_outputs.contains(&"dist/output_a.txt".to_string()));
    }

    // ------------------------------------------------------------------
    // Test 6: format_capsule is bounded
    // ------------------------------------------------------------------

    #[test]
    fn format_capsule_respects_budget() {
        let capsule = WorkspaceCapsule {
            known_files: (0..100)
                .map(|i| CapsuleFileEntry {
                    path: format!("src/really/long/path/to/file_{i}.rs"),
                    content_hash: format!("hash_{i}_with_extra_data_to_fill_space"),
                    last_modified_gate_seq: i as u64,
                })
                .collect(),
            modified_files: Vec::new(),
            pending_work: Vec::new(),
            expected_outputs: Vec::new(),
        };

        let formatted = format_capsule(&capsule);
        assert!(formatted.starts_with("<workspace_capsule>"));
        // Total output is bounded — the clip may truncate the closing tag.
        assert!(
            formatted.len() <= MAX_CAPSULE_CHARS + 200,
            "formatted capsule {} chars exceeds budget {}",
            formatted.len(),
            MAX_CAPSULE_CHARS,
        );
    }

    // ------------------------------------------------------------------
    // Test 7: round-trip serde
    // ------------------------------------------------------------------

    #[test]
    fn round_trip_serde() {
        let capsule = WorkspaceCapsule {
            known_files: vec![CapsuleFileEntry {
                path: "src/lib.rs".into(),
                content_hash: "abc123".into(),
                last_modified_gate_seq: 42,
            }],
            modified_files: vec![CapsuleFileEntry {
                path: "src/old.rs".into(),
                content_hash: "def456".into(),
                last_modified_gate_seq: 10,
            }],
            pending_work: vec![CapsulePendingTask {
                task_id: "task-1".into(),
                description: "implement feature".into(),
                dependencies: vec!["task-0".into()],
            }],
            expected_outputs: vec!["dist/out.txt".into()],
        };

        let json = serde_json::to_string(&capsule).expect("serialize");
        let restored: WorkspaceCapsule = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.known_files.len(), 1);
        assert_eq!(restored.known_files[0].path, "src/lib.rs");
        assert_eq!(restored.modified_files.len(), 1);
        assert_eq!(restored.pending_work.len(), 1);
        assert_eq!(restored.expected_outputs.len(), 1);
    }

    // ------------------------------------------------------------------
    // Test 8: empty capsule formats to empty string
    // ------------------------------------------------------------------

    #[test]
    fn empty_capsule_formats_to_empty() {
        let capsule = WorkspaceCapsule {
            known_files: Vec::new(),
            modified_files: Vec::new(),
            pending_work: Vec::new(),
            expected_outputs: Vec::new(),
        };
        let formatted = format_capsule(&capsule);
        assert!(formatted.is_empty());
    }

    // ------------------------------------------------------------------
    // Test 9: description truncation in pending_work
    // ------------------------------------------------------------------

    #[test]
    fn pending_description_clipped_to_max_entry_chars() {
        let projection = empty_projection();
        let mut graph = TaskGraph::new();
        let long_desc = "a".repeat(1000);
        let t1 = make_task("01HXYZ00000000000000000040", &long_desc, SubTaskStatus::Pending);
        graph.add_root(t1);

        let tid = task_id("01HXYZ00000000000000000040");
        let capsule = build_capsule(&projection, &tid, &graph, &[]);

        // The current task is excluded from pending_work, so it should be empty.
        assert!(capsule.pending_work.is_empty());

        // But the description on the SubTask itself is not truncated — only
        // the format output clips. Let's verify with a different task as pending.
        let mut t2 = make_task("01HXYZ00000000000000000041", &long_desc, SubTaskStatus::Pending);
        t2.dependencies.push(tid);
        graph.add_child(t2, tid, crate::graph::Dependency::MustFinishBefore);

        let capsule = build_capsule(&projection, &tid, &graph, &[]);
        assert_eq!(capsule.pending_work.len(), 1);
        // Description is truncated in the builder.
        assert!(
            capsule.pending_work[0].description.len() <= MAX_ENTRY_CHARS,
            "description should be clipped"
        );
    }
}
