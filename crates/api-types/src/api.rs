//! API request/response types for the Concerto HTTP API.
//!
//! Phase 3: minimal task submission, session management, and spend querying.

use concerto_core::ids::Ulid;
use concerto_core::types::TaskId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Request to create a new session.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    pub provider: String,
    pub model: String,
    /// The project directory for the session. Must be a valid, existing directory.
    /// Cannot be empty. Must be valid UTF-8.
    pub project_dir: String,
}

/// Request to create a new task for an agent session.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateTaskRequest {
    pub description: String,
}

/// Response to a task creation request.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TaskResponse {
    #[schema(value_type = String)]
    pub task_id: TaskId,
    #[schema(value_type = String)]
    pub session_id: Ulid,
    pub status: String,
}

/// Summary of a session.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionResponse {
    #[schema(value_type = String)]
    pub id: Ulid,
    pub created_at: String,
    pub last_activity: String,
    pub provider: String,
    pub model: String,
}

/// Spend summary for a session.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SpendSummaryResponse {
    #[schema(value_type = String)]
    pub session_id: Ulid,
    pub total_usd: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test CreateSessionRequest serialization round-trip.
    #[test]
    fn create_session_request_round_trip() {
        let req = CreateSessionRequest {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            project_dir: "/tmp/project".to_string(),
        };
        let json = serde_json::to_string(&req).expect("serialization should succeed");
        let deserialized: CreateSessionRequest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.provider, "openai");
        assert_eq!(deserialized.model, "gpt-4");
        assert_eq!(deserialized.project_dir, "/tmp/project");
    }

    /// Test CreateSessionRequest with empty strings.
    #[test]
    fn create_session_request_empty_strings() {
        let req = CreateSessionRequest {
            provider: String::new(),
            model: String::new(),
            project_dir: String::new(),
        };
        let json = serde_json::to_string(&req).expect("serialization should succeed");
        let deserialized: CreateSessionRequest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert!(deserialized.provider.is_empty());
        assert!(deserialized.model.is_empty());
        assert!(deserialized.project_dir.is_empty());
    }

    /// Test CreateTaskRequest serialization round-trip.
    #[test]
    fn create_task_request_round_trip() {
        let req = CreateTaskRequest { description: "Fix the bug".to_string() };
        let json = serde_json::to_string(&req).expect("serialization should succeed");
        let deserialized: CreateTaskRequest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.description, "Fix the bug");
    }

    /// Test CreateTaskRequest with multi-line description.
    #[test]
    fn create_task_request_multiline_description() {
        let req = CreateTaskRequest { description: "Line 1\nLine 2\nLine 3".to_string() };
        let json = serde_json::to_string(&req).expect("serialization should succeed");
        let deserialized: CreateTaskRequest =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.description, "Line 1\nLine 2\nLine 3");
    }

    /// Test TaskResponse serialization with TaskId and Ulid.
    #[test]
    fn task_response_serialization() {
        let resp = TaskResponse {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            status: "running".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("serialization should succeed");
        assert!(json.contains("\"task_id\""));
        assert!(json.contains("\"session_id\""));
        assert!(json.contains("\"status\":\"running\""));
        let deserialized: TaskResponse =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.status, "running");
    }

    /// Test SessionResponse serialization with Ulid and datetime strings.
    #[test]
    fn session_response_serialization() {
        let resp = SessionResponse {
            id: Ulid::new(),
            created_at: "2026-07-28T12:00:00Z".to_string(),
            last_activity: "2026-07-28T12:05:00Z".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-3".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("serialization should succeed");
        let deserialized: SessionResponse =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.provider, "anthropic");
        assert_eq!(deserialized.model, "claude-3");
        assert_eq!(deserialized.created_at, "2026-07-28T12:00:00Z");
    }

    /// Test SpendSummaryResponse serialization with f64 precision.
    #[test]
    fn spend_summary_response_f64_precision() {
        let resp = SpendSummaryResponse {
            session_id: Ulid::new(),
            total_usd: 0.123456789,
            total_tokens_in: 1000,
            total_tokens_out: 500,
        };
        let json = serde_json::to_string(&resp).expect("serialization should succeed");
        let deserialized: SpendSummaryResponse =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert!((deserialized.total_usd - 0.123456789).abs() < 1e-9);
        assert_eq!(deserialized.total_tokens_in, 1000);
        assert_eq!(deserialized.total_tokens_out, 500);
    }

    /// Test SpendSummaryResponse with zero values.
    #[test]
    fn spend_summary_response_zero_values() {
        let resp = SpendSummaryResponse {
            session_id: Ulid::new(),
            total_usd: 0.0,
            total_tokens_in: 0,
            total_tokens_out: 0,
        };
        let json = serde_json::to_string(&resp).expect("serialization should succeed");
        let deserialized: SpendSummaryResponse =
            serde_json::from_str(&json).expect("deserialization should succeed");
        assert_eq!(deserialized.total_usd, 0.0);
        assert_eq!(deserialized.total_tokens_in, 0);
        assert_eq!(deserialized.total_tokens_out, 0);
    }

    /// Test that all API types implement Clone correctly.
    #[test]
    fn api_types_clone() {
        let req1 = CreateSessionRequest {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            project_dir: "/tmp".to_string(),
        };
        let req2 = req1.clone();
        assert_eq!(req1.provider, req2.provider);
        assert_eq!(req1.model, req2.model);
    }

    /// Test that all API types implement Debug correctly.
    #[test]
    fn api_types_debug() {
        let req = CreateSessionRequest {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            project_dir: "/tmp".to_string(),
        };
        let debug_str = format!("{:?}", req);
        assert!(debug_str.contains("CreateSessionRequest"));
        assert!(debug_str.contains("openai"));
    }
}
