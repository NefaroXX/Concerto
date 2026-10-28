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
    store: Option<&Arc<dyn SessionStore>>,
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
    if let Some(store) = store {
        let message = Message {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        if let Err(error) = store.append_messages(task.session_id, &[message], cancel.clone()).await
        {
            return json!({"error": format!("could not save reply: {error}")});
        }
    }
    let _ = bus.publish_for_session(
        task.session_id,
        task.id.0,
        EventKind::AssistantMessage { task_id: task.id, content: content.into() },
    );
    json!({"delivered": true, "continue_work": true})
}
