//! Provider-native response state that cannot be reconstructed from display text.

use crate::error::ProviderError;
use crate::types::{Message, Role, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Per-turn limits include opaque signatures and unknown native part fields.
pub const MAX_GEMINI_REPLAY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_GEMINI_REPLAY_PARTS: usize = 8192;

/// Exact Gemini parts, in arrival order. The digest binds these parts to the
/// canonical response: edited messages must never replay stale model output.
/// It is an integrity check, not an authentication or authorization mechanism.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiReplay {
    parts: Vec<Value>,
    canonical_digest: String,
}

impl GeminiReplay {
    pub fn capture(
        parts: Vec<Value>,
        text: &str,
        reasoning: Option<&str>,
        calls: &[ToolCall],
    ) -> Result<Self, ProviderError> {
        let replay = Self {
            parts,
            canonical_digest: canonical_digest(text, reasoning, calls)
                .map_err(|error| ProviderError::Serialization(error.to_string()))?,
        };
        if replay.parts.len() > MAX_GEMINI_REPLAY_PARTS
            || replay.encoded_len() > MAX_GEMINI_REPLAY_BYTES
        {
            return Err(ProviderError::InvalidResponse(
                "Gemini response exceeds native replay limits".into(),
            ));
        }
        Ok(replay)
    }

    /// Invalid or stale persisted replay state falls back to the canonical
    /// message. Compaction explicitly clears the state when it removes calls.
    pub fn parts_for(&self, message: &Message) -> Option<&[Value]> {
        if message.role != Role::Assistant
            || message.tool_results.is_some()
            || self.parts.len() > MAX_GEMINI_REPLAY_PARTS
            || self.encoded_len() > MAX_GEMINI_REPLAY_BYTES
        {
            return None;
        }
        let digest = canonical_digest(
            &message.content,
            message.reasoning_content.as_deref(),
            message.tool_calls.as_deref().unwrap_or_default(),
        )
        .ok()?;
        (digest == self.canonical_digest).then_some(self.parts.as_slice())
    }

    /// Conservative context accounting includes signatures and all native fields.
    pub fn encoded_len(&self) -> usize {
        serde_json::to_vec(&self.parts).map_or(usize::MAX, |bytes| bytes.len())
    }
}

fn canonical_digest(
    text: &str,
    reasoning: Option<&str>,
    calls: &[ToolCall],
) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(&(text, reasoning.filter(|r| !r.trim().is_empty()), calls))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CompletionChunk;
    use serde_json::json;

    #[test]
    fn replay_regression_legacy_json_omits_optional_state() {
        let message: Message = serde_json::from_value(json!({
            "role":"Assistant", "content":"legacy", "tool_calls":null, "tool_results":null
        }))
        .unwrap();
        assert!(message.gemini_replay.is_none());
        assert!(serde_json::to_value(&message).unwrap().get("gemini_replay").is_none());
        let chunk: CompletionChunk = serde_json::from_value(json!({
            "delta":"legacy", "reasoning":null, "tool_call":null, "is_final":true, "usage":null
        }))
        .unwrap();
        assert!(chunk.gemini_replay.is_none());
        assert!(serde_json::to_value(&chunk).unwrap().get("gemini_replay").is_none());
    }

    #[test]
    fn replay_regression_digest_covers_call_arguments_and_reasoning() {
        let call = ToolCall {
            id: "id".into(),
            name: "read".into(),
            arguments: json!({"path":"a"}),
            thought_signature: None,
        };
        let mut message = Message {
            role: Role::Assistant,
            content: "answer".into(),
            tool_calls: Some(vec![call.clone()]),
            tool_results: None,
            reasoning_content: Some("summary".into()),
            tokens_in: None,
            tokens_out: None,
            gemini_replay: None,
        };
        let replay = GeminiReplay::capture(
            vec![json!({"text":"answer"})],
            "answer",
            Some("summary"),
            &[call],
        )
        .unwrap();
        assert!(replay.parts_for(&message).is_some());
        message.reasoning_content = None;
        assert!(replay.parts_for(&message).is_none());
        message.reasoning_content = Some("summary".into());
        message.tool_calls.as_mut().unwrap()[0].arguments = json!({"path":"different"});
        assert!(replay.parts_for(&message).is_none());
    }

    #[test]
    fn replay_regression_capture_enforces_part_and_byte_bounds() {
        assert!(
            GeminiReplay::capture(vec![json!({}); MAX_GEMINI_REPLAY_PARTS], "", None, &[]).is_ok()
        );
        assert!(GeminiReplay::capture(vec![json!({}); MAX_GEMINI_REPLAY_PARTS + 1], "", None, &[])
            .is_err());
        assert!(GeminiReplay::capture(
            vec![json!({"text":"x".repeat(MAX_GEMINI_REPLAY_BYTES)})],
            "",
            None,
            &[]
        )
        .is_err());
    }
}
