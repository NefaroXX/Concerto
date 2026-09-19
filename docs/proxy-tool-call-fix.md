# Proxy Tool-Call Fix for OpenRouter / NIM / OpenAI-Compatible Providers

> **Status:** Partially implemented (2026-09-20). Fix 1 (flat proxy format)
> is done — `openai.rs:326` flat fallback + tests at `:781`, `:815`, `:841`.
> Fix 2 (content-embedded, strict) is done — content buffered to turn end,
> full-text envelope parse only when `partial_tools` is empty, real deltas
> always win. Fix 3 (warn/retry) is done — malformed args retry
> trim + single-quote fixup, then warn (name/len/error, never payload) +
> Null. Do
> not use this document as evidence that a proxy/model combination is
> supported.

This document describes a possible issue with non-OpenAI models accessed through
OpenAI-compatible proxy endpoints (OpenRouter, NVIDIA NIM, Together AI, etc.)
and proposed code changes. Any implementation must be backed by captured,
sanitized provider fixtures; permissive parsing can otherwise execute text that
was never intended as a tool call.

## Symptom

When using a model like `kimi-k2.6` through the NIM or OpenRouter provider:

- Tool calls are silently dropped or arrive malformed
- The assistant produces text instead of executing the tool
- Language mixing / register confusion in responses
- These work fine when the same model is used through its native provider

## Root Cause

Concerto's `OpenAiStreamState::handle_event` in
`crates/providers/src/openai.rs` expects tool calls **exclusively** in this
path:

```
choice["delta"]["tool_calls"][i]["function"]["arguments"]
```

But proxy gateways translate between the upstream model's native format and the
OpenAI wire format. This translation has three common failure modes:

| # | Failure | What happens |
|---|---------|-------------|
| 1 | **Flat format** — proxy emits `{id, name, arguments}` directly on the tool call object, not nested under `function` | `tc.get("function")` returns `None`, tool call silently dropped |
| 2 | **Tool calls in content** — proxy serializes the tool call as a JSON string inside `delta.content` instead of using the structured `delta.tool_calls` array | The `delta.content` handler treats it as text, appends it to the assistant message, and never triggers a tool execution |
| 3 | **Non-string arguments** — proxy sends `arguments` as a JSON object or array instead of an incremental string, or sends malformed concatenated fragments | `emit_tool_call` silently fails `serde_json::from_str` and produces `Value::Null` with no diagnostic |

## Fix

All changes go in `crates/providers/src/openai.rs`.

### Fix 1: Support flat tool call format (no `function` wrapper) — ✅ DONE

**Implemented:** `crates/providers/src/openai.rs:326` — flat fallback after the
standard `function`-wrapper block. Tests at `:781` (string args), `:815`
(object args), `:841` (`input` alias). The implementation gates conservatively
on the tool call carrying both a string `name` AND `arguments` (string or
object) so unrelated payloads cannot be misread. The `emit_tool_call` path
normalizes arguments to a JSON object via `ensure_arguments_object`.

### Fix 2: Extract tool calls from `delta.content` — ✅ DONE (strict)

Some proxies embed the entire tool call as a JSON string in the `content` field
rather than using the structured `tool_calls` array. Add this fallback **after**
the `tool_calls` array block (after the `for tc in tc_arr` loop ends, around
line 129) and **before** the `finish_reason` check.

**Add after line 129:**

```rust
// ── Proxy fallback: tool calls embedded in content as JSON ──
// Only activate when the structured path found nothing.
if self.partial_tools.is_empty() {
    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
        let trimmed = content.trim();
        if trimmed.starts_with('{') {
            if let Ok(content_val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                let tc_type = content_val.get("type").and_then(|v| v.as_str());
                if tc_type == Some("function")
                    || tc_type == Some("tool_use")
                    || tc_type == Some("tool_call")
                {
                    let index = 0;
                    let partial = self.partial_tools.entry(index).or_default();
                    if let Some(id) = content_val.get("id").and_then(|v| v.as_str()) {
                        partial.id = id.to_string();
                    }
                    // Try name at top level or inside a "function" wrapper
                    if let Some(name) = content_val
                        .get("name")
                        .or_else(|| {
                            content_val
                                .get("function")
                                .and_then(|f| f.get("name"))
                        })
                        .and_then(|v| v.as_str())
                    {
                        partial.name = name.to_string();
                    }
                    // Try arguments at top level, "input", or inside "function"
                    let args_val = content_val
                        .get("arguments")
                        .or_else(|| content_val.get("input"))
                        .or_else(|| {
                            content_val
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                        });
                    if let Some(args) = args_val {
                        if let Some(s) = args.as_str() {
                            partial.arguments.push_str(s);
                        } else if let Ok(json_str) = serde_json::to_string(args) {
                            partial.arguments = json_str;
                        }
                    }
                    // Don't emit this content as text — it's a tool call
                    continue;
                }
            }
        }
    }
}
```

**Important:** The `continue` means this code must be inside the `for tc in tc_arr`
loop (or restructured as a separate check before/after). If adding inline is awkward,
wrap the entire block in a helper method `try_extract_tool_call_from_content`.
The `continue` skips falling through to emit text, so the tool call isn't
duplicated as both a tool call and assistant text.

### Fix 3: Better error recovery and logging in `emit_tool_call` — ✅ DONE

In `OpenAiStreamState::emit_tool_call`, the current code silently discards parse
failures. Replace it with diagnostic logging and a retry.

**Current (line 162):** uses `ensure_arguments_object` to coerce parse failures
to `{}` rather than diagnostic logging. The proposed replacement adds
`tracing::warn!` with the raw payload and a single-quote fixup retry.

**Replace with:**

```rust
fn emit_tool_call(&mut self, index: usize) {
    if let Some(ptc) = self.partial_tools.remove(&index) {
        let args = if ptc.arguments.trim().is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_str(&ptc.arguments) {
                Ok(v) => v,
                Err(parse_err) => {
                    // Proxy sent malformed JSON arguments.
                    // Try a few common fixes before giving up.
                    let cleaned = ptc.arguments.trim();
                    // Some proxies double-wrap: "{"key": "val"}" → try as-is
                    let result = serde_json::from_str(cleaned).or_else(|_| {
                        // Some proxies send single-quoted keys
                        let fixed = cleaned.replace('\'', "\"");
                        serde_json::from_str(&fixed)
                    });
                    match result {
                        Ok(v) => v,
                        Err(_) => {
                            tracing::warn!(
                                tool_name = %ptc.name,
                                raw_len = ptc.arguments.len(),
                                parse_error = %parse_err,
                                "emit_tool_call: failed to parse arguments, \
                                 emitting null."
                            );
                            serde_json::Value::Null
                        }
                    }
                }
            }
        };
        self.pending.push_back(Ok(CompletionChunk {
            delta: String::new(),
            tool_call: Some(ToolCall { id: ptc.id, name: ptc.name, arguments: args }),
            is_final: false,
        }));
    }
}
```

## Verification

Fix 1 is verified by unit tests (`:781`, `:815`, `:841`) covering string
fragments, object one-shot, and `input` alias. To test live:

```bash
CONCERTO_LOG=warn cargo run -p concerto-desktop
# or
RUST_LOG=warn cargo run -p concerto-cli
```

If tool calls still fail, the `tracing::warn!` in Fix 3 (when implemented)
will print the raw arguments payload. Open an issue with that output.

## Design Notes

- All three fixes are **backward-compatible** — they only activate when the
  standard OpenAI path finds nothing.
- Fix 2's `continue` is important: it prevents the content from appearing both
  as tool call text AND as a structured tool call.
- If the proxy sends partial tool call fragments in both `content` and
  `tool_calls` simultaneously, Fix 2 should be gated on
  `self.partial_tools.is_empty()` (already shown above).
