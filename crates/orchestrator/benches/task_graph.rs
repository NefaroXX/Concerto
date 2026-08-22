//! Benchmarks for TaskGraph operations.

use concerto_core::ids::Ulid;
use concerto_core::types::{AgentId, TaskId};
use concerto_orchestrator::graph::{Dependency, TaskGraph};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn make_task(_id: TaskId, role: AgentId) -> concerto_core::types::SubTask {
    concerto_core::types::SubTask::new(Ulid::new(), role, "benchmark task")
}

fn bench_task_graph(c: &mut Criterion) {
    let mut group = c.benchmark_group("task_graph");

    group.bench_function("ready_tasks_single_root", |b| {
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(TaskId::new(), AgentId::new("architect")));
        b.iter(|| {
            let ready = graph.ready_tasks();
            black_box(ready.len());
        });
    });

    group.bench_function("ready_tasks_chain_10", |b| {
        let mut graph = TaskGraph::new();
        let mut prev = TaskId::new();
        graph.add_root(make_task(prev, AgentId::new("architect")));
        for i in 0..10 {
            let id = TaskId::new();
            graph.add_child(
                make_task(id, AgentId::new("coder")),
                prev,
                Dependency::MustFinishBefore,
            );
            prev = id;
            if i % 2 == 0 {
                graph.mark_done(&prev);
            }
        }
        b.iter(|| {
            let ready = graph.ready_tasks();
            black_box(ready.len());
        });
    });

    group.bench_function("all_completed_false", |b| {
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(TaskId::new(), AgentId::new("architect")));
        b.iter(|| {
            let done = graph.all_completed();
            black_box(done);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_task_graph);
criterion_main!(benches);
