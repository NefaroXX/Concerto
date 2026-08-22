//! Benchmark for LLM provider streaming throughput.
//!
//! Uses `MockProvider` to measure the end-to-end overhead of
//! `stream_completion` dispatch, stream creation, and iteration
//! without real network I/O.

use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{CompletionRequest, Message, Role, ToolCall};
use concerto_core::CancellationToken;
use concerto_providers::mock::MockProvider;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures::StreamExt;

/// Build a CompletionRequest with `n` messages of roughly realistic size.
fn build_request(n: usize, tool_calls: bool) -> CompletionRequest {
    let messages: Vec<Message> = (0..n)
        .map(|i| {
            let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
            Message {
                role,
                content: format!(
                    "This is conversation message number {i}. It simulates realistic \
                     content that would appear in a coding assistant conversation, \
                     including references to files, code snippets, and explanations \
                     of programming concepts. The goal is to have realistic-sized \
                     messages for benchmarking provider streaming throughput."
                ),
                tool_calls: if tool_calls && i % 3 == 0 {
                    Some(vec![
                        ToolCall {
                            id: format!("call_{i}_a"),
                            name: "read_file".into(),
                            arguments: serde_json::json!({"path": "/src/main.rs"}),
                        },
                        ToolCall {
                            id: format!("call_{i}_b"),
                            name: "write_file".into(),
                            arguments: serde_json::json!({"path": "/src/lib.rs", "content": "pub fn hello() {}"}),
                        },
                    ])
                } else {
                    None
                },
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }
        })
        .collect();
    CompletionRequest {
        model: "gpt-4o".into(),
        messages,
        tools: None,
        tool_choice: None,
        temperature: Some(0.7),
        max_tokens: Some(4096),
        stream: true,
    }
}

fn bench_provider_streaming(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cancel = CancellationToken::new();

    let mut group = c.benchmark_group("provider_streaming");

    // --- Mock with zero latency, varying message counts ---
    for (name, msg_count) in [("2msg", 2), ("20msg", 20), ("100msg", 100)] {
        let provider = MockProvider::default();
        let request = build_request(msg_count, false);
        let bench_name = format!("zero_latency/{name}");
        group.bench_function(&bench_name, |b| {
            b.to_async(&rt).iter(|| {
                let p = &provider;
                let req = request.clone();
                let c = cancel.clone();
                async move {
                    let mut stream = p.stream_completion(req, c).await.unwrap();
                    while let Some(chunk) = stream.next().await {
                        black_box(chunk.unwrap());
                    }
                }
            });
        });
    }

    // --- Mock with tool-call messages (20 msg) ---
    {
        let provider = MockProvider::default();
        let request = build_request(20, true);
        group.bench_function("zero_latency/20msg_with_tool_calls", |b| {
            b.to_async(&rt).iter(|| {
                let p = &provider;
                let req = request.clone();
                let c = cancel.clone();
                async move {
                    let mut stream = p.stream_completion(req, c).await.unwrap();
                    while let Some(chunk) = stream.next().await {
                        black_box(chunk.unwrap());
                    }
                }
            });
        });
    }

    // --- 5ms simulated latency (2 messages) ---
    {
        let mut provider = MockProvider::default();
        provider.latency_ms = 5;
        let request = build_request(2, false);
        group.bench_function("latency_5ms/2msg", |b| {
            b.to_async(&rt).iter(|| {
                let p = &provider;
                let req = request.clone();
                let c = cancel.clone();
                async move {
                    let mut stream = p.stream_completion(req, c).await.unwrap();
                    while let Some(chunk) = stream.next().await {
                        black_box(chunk.unwrap());
                    }
                }
            });
        });
    }

    // --- 10ms latency (2 messages) ---
    {
        let mut provider = MockProvider::default();
        provider.latency_ms = 10;
        let request = build_request(2, false);
        group.bench_function("latency_10ms/2msg", |b| {
            b.to_async(&rt).iter(|| {
                let p = &provider;
                let req = request.clone();
                let c = cancel.clone();
                async move {
                    let mut stream = p.stream_completion(req, c).await.unwrap();
                    while let Some(chunk) = stream.next().await {
                        black_box(chunk.unwrap());
                    }
                }
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_provider_streaming);
criterion_main!(benches);
