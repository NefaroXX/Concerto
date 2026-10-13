//! Explicit Anthropic cache placement, after route-specific rendering.
//! Four breakpoints at most; existing manual markers are preserved.

use serde_json::{json, Value};

fn markers(value: &Value) -> usize {
    match value {
        Value::Object(object) => {
            usize::from(object.contains_key("cache_control"))
                + object.values().map(markers).sum::<usize>()
        }
        Value::Array(array) => array.iter().map(markers).sum(),
        _ => 0,
    }
}

fn mark(block: &mut Value, remaining: &mut usize) {
    if *remaining == 0 || block.get("cache_control").is_some() {
        return;
    }
    block["cache_control"] = json!({"type": "ephemeral"});
    *remaining -= 1;
}

pub(crate) fn apply_anthropic(body: &mut Value) {
    // Automatic top-level caching owns its boundary; leave it alone.
    if body.get("cache_control").is_some() {
        return;
    }
    if let Some(system) = body.get("system").and_then(Value::as_str) {
        body["system"] = json!([{"type": "text", "text": system}]);
    }
    let mut remaining = 4usize.saturating_sub(markers(body));
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        if let Some(last) = tools.last_mut() {
            mark(last, &mut remaining);
        }
    }
    if let Some(system) = body.get_mut("system") {
        if let Some(blocks) = system.as_array_mut() {
            if let Some(first) = blocks.first_mut() {
                mark(first, &mut remaining);
            }
            if let Some(last) = blocks.last_mut() {
                mark(last, &mut remaining);
            }
        } else if system.is_object() {
            mark(system, &mut remaining);
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut().rev() {
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                if let Some(block) = content.iter_mut().rev().find(|block| {
                    matches!(block["type"].as_str(), Some("text" | "tool_use" | "tool_result"))
                }) {
                    mark(block, &mut remaining);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_tail_and_stable_system_fit_the_budget_and_are_idempotent() {
        let mut body = json!({
            "tools": [{"name": "read", "input_schema": {}}],
            "system": [{"type": "text", "text": "stable"}, {"type": "text", "text": "volatile"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "old"}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "result"}]}
            ]
        });
        apply_anthropic(&mut body);
        assert_eq!(markers(&body), 4);
        assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
        assert!(body["messages"][1]["content"][0].get("cache_control").is_some());
        let once = body.clone();
        apply_anthropic(&mut body);
        assert_eq!(body, once);
    }

    #[test]
    fn manual_markers_and_thinking_blocks_are_preserved() {
        let mut body = json!({"messages": [{"role": "assistant", "content": [
            {"type": "text", "text": "answer", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            {"type": "thinking", "thinking": "private", "signature": "opaque"}
        ]}]});
        let before = body.clone();
        apply_anthropic(&mut body);
        assert_eq!(body, before);
    }
}
