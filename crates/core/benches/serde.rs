//! Benchmark for serde round-trips of core message types.
//!
//! Measures JSON serialization and deserialization throughput for
//! `Message`, `ToolCall`, `ToolResult`, and `ToolDefinition` — types
//! that are serialized on every provider interaction.
//!
//! Note: `CompletionRequest` and `CompletionChunk` do not derive
//! serde (they are internal provider-flow types), so we benchmark
//! the serde-heavy leaf types only.

use concerto_core::types::{Message, Role, ToolCall, ToolDefinition, ToolResult};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

/// Build N messages alternating user/assistant roles, some with tool calls.
fn build_messages(n: usize) -> Vec<Message> {
    (0..n)
        .map(|i| {
            let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
            Message {
                role,
                content: format!(
                    "This is message number {i} with some reasonably long content \
                     that simulates a real conversation turn in the chat interface."
                ),
                tool_calls: if i % 3 == 0 {
                    Some(vec![
                        ToolCall {
                            id: format!("call_{i}_1"),
                            name: "read_file".into(),
                            arguments: serde_json::json!({"path": format!("/path/to/file_{i}.rs")}),
                        },
                        ToolCall {
                            id: format!("call_{i}_2"),
                            name: "write_file".into(),
                            arguments: serde_json::json!({
                                "path": format!("/path/to/output_{i}.rs"),
                                "content": "fn main() { println!(\"hello\"); }"
                            }),
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
        .collect()
}

/// Build a ToolDefinition with representative schema size.
fn build_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".into(),
            description: "Read a file from the filesystem".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to read"}
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "write_file".into(),
            description: "Write content to a file at the specified path.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"},
                    "content": {"type": "string", "description": "Content to write"}
                },
                "required": ["path", "content"]
            }),
        },
        ToolDefinition {
            name: "search_code".into(),
            description: "Search the codebase for matching patterns using semantic search.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"},
                    "max_results": {"type": "integer", "description": "Max results", "default": 10}
                },
                "required": ["query"]
            }),
        },
    ]
}

fn bench_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("serde");

    // --- Single message ---
    let msg = Message {
        role: Role::User,
        content: "What is the capital of France?".into(),
        tool_calls: None,
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
    };
    group.bench_function("serialize/single_message", |b| {
        b.iter(|| serde_json::to_string(black_box(&msg)).unwrap());
    });
    let msg_json = serde_json::to_string(&msg).unwrap();
    group.bench_function("deserialize/single_message", |b| {
        b.iter(|| {
            let _: Message = serde_json::from_str(black_box(&msg_json)).unwrap();
        });
    });

    // --- Message with tool calls ---
    let msg_with_tc = Message {
        role: Role::Assistant,
        content: "Let me check that file for you.".into(),
        tool_calls: Some(vec![ToolCall {
            id: "call_abc".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        }]),
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
    };
    group.bench_function("serialize/message_with_tool_call", |b| {
        b.iter(|| serde_json::to_string(black_box(&msg_with_tc)).unwrap());
    });
    let mtc_json = serde_json::to_string(&msg_with_tc).unwrap();
    group.bench_function("deserialize/message_with_tool_call", |b| {
        b.iter(|| {
            let _: Message = serde_json::from_str(black_box(&mtc_json)).unwrap();
        });
    });

    // --- Message list (2 messages) ---
    let msgs_2 = build_messages(2);
    group.bench_function("serialize/2_messages", |b| {
        b.iter(|| serde_json::to_string(black_box(&msgs_2)).unwrap());
    });
    let msgs_2_json = serde_json::to_string(&msgs_2).unwrap();
    group.bench_function("deserialize/2_messages", |b| {
        b.iter(|| {
            let _: Vec<Message> = serde_json::from_str(black_box(&msgs_2_json)).unwrap();
        });
    });

    // --- Message list (20 messages) ---
    let msgs_20 = build_messages(20);
    group.bench_function("serialize/20_messages", |b| {
        b.iter(|| serde_json::to_string(black_box(&msgs_20)).unwrap());
    });
    let msgs_20_json = serde_json::to_string(&msgs_20).unwrap();
    group.bench_function("deserialize/20_messages", |b| {
        b.iter(|| {
            let _: Vec<Message> = serde_json::from_str(black_box(&msgs_20_json)).unwrap();
        });
    });

    // --- Message list (100 messages) ---
    let msgs_100 = build_messages(100);
    group.bench_function("serialize/100_messages", |b| {
        b.iter(|| serde_json::to_string(black_box(&msgs_100)).unwrap());
    });
    let msgs_100_json = serde_json::to_string(&msgs_100).unwrap();
    group.bench_function("deserialize/100_messages", |b| {
        b.iter(|| {
            let _: Vec<Message> = serde_json::from_str(black_box(&msgs_100_json)).unwrap();
        });
    });

    // --- ToolDefinition list (3 tools) ---
    let defs = build_tool_definitions();
    group.bench_function("serialize/3_tool_definitions", |b| {
        b.iter(|| serde_json::to_string(black_box(&defs)).unwrap());
    });
    let defs_json = serde_json::to_string(&defs).unwrap();
    group.bench_function("deserialize/3_tool_definitions", |b| {
        b.iter(|| {
            let _: Vec<ToolDefinition> = serde_json::from_str(black_box(&defs_json)).unwrap();
        });
    });

    // --- ToolResult ---
    let tool_result = ToolResult {
        id: "call_xyz".into(),
        name: "read_file".into(),
        content: serde_json::json!({"content": "fn main() {}\n", "path": "src/main.rs"}),
    };
    group.bench_function("serialize/tool_result", |b| {
        b.iter(|| serde_json::to_string(black_box(&tool_result)).unwrap());
    });
    let tr_json = serde_json::to_string(&tool_result).unwrap();
    group.bench_function("deserialize/tool_result", |b| {
        b.iter(|| {
            let _: ToolResult = serde_json::from_str(black_box(&tr_json)).unwrap();
        });
    });

    // --- Single ToolCall ---
    let tc = ToolCall {
        id: "call_def456".into(),
        name: "edit_file".into(),
        arguments: serde_json::json!({"path": "src/main.rs", "content": "fn main() { println!(\"updated\"); }"}),
    };
    group.bench_function("serialize/single_tool_call", |b| {
        b.iter(|| serde_json::to_string(black_box(&tc)).unwrap());
    });
    let tc_json = serde_json::to_string(&tc).unwrap();
    group.bench_function("deserialize/single_tool_call", |b| {
        b.iter(|| {
            let _: ToolCall = serde_json::from_str(black_box(&tc_json)).unwrap();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_serde);
criterion_main!(benches);
