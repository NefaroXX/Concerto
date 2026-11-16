//! A visible conversational reply does not terminate the work loop.

use super::*;
use concerto_core::types::{AgentTask, ToolDefinition};
use serde_json::{json, Value};

pub(crate) const REPLY_TOOL: &str = "reply_to_user";

pub(crate) fn reply_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: REPLY_TOOL.into(),
        description: "Send the user an answer or progress update and KEEP WORKING. Use for status questions and informative follow-ups that do not cancel the objective. This does not complete, pause, or authorize any task.".into(),
        parameters: json!({
            "type": "object",
            "properties": {"message": {"type": "string"}},
            "required": ["message"],
            "additionalProperties": false
        }),
    }
}

pub(crate) async fn reply_to_user(
    arguments: &Value,
    bus: &EventBus,
    task: &AgentTask,
    cancel: &CancellationToken,
) -> Value {
    let Some(content) = arguments.get("message").and_then(Value::as_str) else {
        return json!({"error": "message must be a string"});
    };
    if content.trim().is_empty() || content.len() > MAX_MESSAGE_BYTES {
        return json!({"error": "message must be nonempty and at most 128 KiB"});
    }
    if cancel.is_cancelled() {
        return json!({"error": "conversation cancelled"});
    }
    // The root coordinator persists the original tool call and its result.
    // The transcript recorder owns this rendered reply; adding an assistant
    // mirror to model history would split the call/result exchange.
    let _ = bus.publish_for_session(
        task.session_id,
        task.id.0,
        EventKind::AssistantMessage { task_id: task.id, content: content.into() },
    );
    json!({"delivered": true, "continue_work": true})
}
