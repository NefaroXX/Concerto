//! Failure classification utilities for Concerto.
//!
//! This module defines a simple classification of errors into user-visible
//! failures and internal developer-focused failures. It provides a `From`
//! implementation to convert `OrchestratorError` into a `ClassifiedFailure`.

use crate::error::{OrchestratorError, ProviderError};

/// Who should see the failure message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureAudience {
    /// Displayed directly to the user.
    User,
    /// Logged for developers – internal bug or unexpected state.
    Developer,
}

/// A structured failure record.
#[derive(Debug, Clone)]
pub struct ClassifiedFailure {
    /// Intended audience.
    pub audience: FailureAudience,
    /// Short error code identifier.
    pub code: String,
    /// Message safe to show to the user.
    pub user_message: String,
    /// Detailed developer-oriented description.
    pub dev_details: String,
}

impl From<OrchestratorError> for ClassifiedFailure {
    fn from(error: OrchestratorError) -> Self {
        // Every arm below initializes all three fields; declaring them
        // uninitialized makes the compiler verify exhaustiveness of the
        // assignments themselves (a missed arm fails the build).
        let audience: FailureAudience;
        let code: String;
        let user_message: String;
        let dev_details = format!("{error:?}");

        match &error {
            OrchestratorError::ExecutionRequiredButNoTools => {
                audience = FailureAudience::User;
                code = "ACTION_REQUIRED_NO_TOOLS".to_string();
                user_message = "The task requires actions but no tool calls were made. Please ensure the prompt requests a tool operation.".to_string();
            }
            OrchestratorError::CycleDetected { tool_name, count } => {
                audience = FailureAudience::User;
                code = "CYCLE_DETECTED".to_string();
                user_message = format!(
                    "The tool '{tool_name}' was called {count} times with the same input, which may indicate a loop."
                );
            }
            OrchestratorError::MaxIterationsReached { max } => {
                audience = FailureAudience::Developer;
                code = "MAX_ITERATIONS_REACHED".to_string();
                user_message = format!(
                    "The agent exceeded the maximum {max} iterations without completing the task."
                );
            }
            OrchestratorError::Cancelled => {
                audience = FailureAudience::User;
                code = "TASK_CANCELLED".to_string();
                user_message = "The task was cancelled by the user or system.".to_string();
            }
            OrchestratorError::SubTaskRetriesExhausted { role, attempts, last_error, .. } => {
                audience = FailureAudience::User;
                code = "SUBTASK_RECOVERY_EXHAUSTED".to_string();
                user_message = format!(
                    "The {role:?} agent could not recover after {attempts} attempts: {last_error}"
                );
            }
            OrchestratorError::InvalidPolicyConfiguration { reason } => {
                audience = FailureAudience::User;
                code = "INVALID_POLICY_CONFIGURATION".to_string();
                user_message = format!("A policy rule is invalid: {reason}");
            }
            OrchestratorError::PinnedModelMissingCapability { role, model, capability } => {
                audience = FailureAudience::User;
                code = "MODEL_CAPABILITY_MISSING".to_string();
                user_message = format!(
                    "The model '{model}' cannot serve {role:?} because it lacks {capability}. Choose a compatible model."
                );
            }
            OrchestratorError::NoCapableModel { role, capability } => {
                audience = FailureAudience::User;
                code = "NO_CAPABLE_MODEL".to_string();
                user_message = format!(
                    "No configured model can serve {role:?}; required capability: {capability}."
                );
            }
            OrchestratorError::PinnedModelNotFound { role, provider_config_id, model } => {
                audience = FailureAudience::User;
                code = "MODEL_ASSIGNMENT_NOT_FOUND".to_string();
                user_message = format!(
                    "The configured model '{model}' for {role:?} was not found in provider configuration {provider_config_id:?}. Check the role assignment."
                );
            }
            OrchestratorError::PinnedModelBudgetExceeded { model, estimated, remaining } => {
                audience = FailureAudience::User;
                code = "MODEL_BUDGET_EXCEEDED".to_string();
                user_message = format!(
                    "The pinned model '{model}' is estimated to cost ${estimated:.4}, but only ${remaining:.4} remains."
                );
            }
            OrchestratorError::NoAffordableModel { role } => {
                audience = FailureAudience::User;
                code = "NO_AFFORDABLE_MODEL".to_string();
                user_message =
                    format!("No configured model for {role:?} fits the remaining spend budget.");
            }
            OrchestratorError::MaxReviewCyclesExceeded { cycles, .. } => {
                audience = FailureAudience::User;
                code = "REVIEW_RECOVERY_EXHAUSTED".to_string();
                user_message =
                    format!("Code review still requires changes after {cycles} revision cycles.");
            }
            OrchestratorError::MaxValidationCyclesExceeded { cycles, .. } => {
                audience = FailureAudience::User;
                code = "VALIDATION_RECOVERY_EXHAUSTED".to_string();
                user_message = format!(
                    "Validation still fails after {cycles} automatic repair cycles. The latest validation output has been preserved."
                );
            }
            OrchestratorError::AgentLoopError(message)
                if is_architect_structured_output_failure(message) =>
            {
                audience = FailureAudience::User;
                code = "AGENT_OUTPUT_INVALID".to_string();
                user_message = "The design agent returned a design response that could not be validated after automatic repair attempts. The session is still usable; retry the design step or select another model.".to_string();
            }
            OrchestratorError::AgentLoopError(message) if is_architect_failure(message) => {
                audience = FailureAudience::User;
                code = "ARCHITECT_AGENT_FAILED".to_string();
                user_message = "The design agent could not complete its assignment. Its diagnostic trace has been preserved, and the session can be retried.".to_string();
            }
            OrchestratorError::AgentLoopError(message) if is_memory_init_failure(message) => {
                audience = FailureAudience::User;
                code = "MEMORY_INIT_FAILED".to_string();
                user_message = format!(
                    "Local memory storage could not be initialized: {message}. Check the store status with `concerto health`."
                );
            }
            OrchestratorError::AgentLoopError(message) => {
                // Generic loop failures keep their developer detail: the code
                // identifies the stage, the detail carries the cause. Never
                // falls through to INTERNAL_ERROR.
                audience = FailureAudience::Developer;
                code = "AGENT_LOOP_FAILED".to_string();
                user_message = format!("The agent loop failed: {message}");
            }
            OrchestratorError::Unrecoverable { message } => {
                audience = FailureAudience::User;
                code = "UNRECOVERABLE_ERROR".to_string();
                user_message = format!("An unrecoverable error occurred: {message}");
            }
            OrchestratorError::TaskGraphError(message) => {
                audience = FailureAudience::User;
                code = "TASK_GRAPH_ERROR".to_string();
                user_message =
                    format!("The task graph could not be executed or advanced: {message}");
            }
            OrchestratorError::Tool(tool_error) => {
                audience = FailureAudience::Developer;
                code = "TOOL_ERROR".to_string();
                user_message = format!("A tool operation failed: {tool_error}");
            }
            OrchestratorError::Memory(memory_error) => {
                audience = FailureAudience::Developer;
                code = "MEMORY_ERROR".to_string();
                user_message = format!("A local memory operation failed: {memory_error}");
            }
            OrchestratorError::MultiAgentPlanFailed { reason } => {
                audience = FailureAudience::User;
                code = "PLANNING_FAILED".to_string();
                user_message = format!("Could not create an execution plan: {reason}");
            }
            OrchestratorError::InvalidTaskGraph { reason } => {
                audience = FailureAudience::Developer;
                code = "INVALID_TASK_GRAPH".to_string();
                user_message = format!("The task graph is invalid: {reason}");
            }
            OrchestratorError::NoBudgetForDelegation => {
                audience = FailureAudience::User;
                code = "BUDGET_EXHAUSTED".to_string();
                user_message =
                    "No remaining budget for delegating tasks. Increase the spend cap or use a cheaper model."
                        .to_string();
            }
            OrchestratorError::Provider(provider_error) => match provider_error {
                ProviderError::Cancelled => {
                    audience = FailureAudience::User;
                    code = "TASK_CANCELLED".to_string();
                    user_message = "The task was cancelled.".to_string();
                }
                ProviderError::AuthFailure => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_AUTH_FAILURE".to_string();
                    user_message = "Authentication with the language model provider failed. Check your API key or credentials.".to_string();
                }
                ProviderError::RateLimit { .. } => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_RATE_LIMIT".to_string();
                    user_message = "The language model provider rate-limited the request. Please try again later.".to_string();
                }
                ProviderError::ContextOverflow { .. } => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_CONTEXT_OVERFLOW".to_string();
                    user_message = "The mandatory request content is larger than the selected model's context window even after automatic compaction. Choose a model with a larger context window or reduce the task's mandatory instructions.".to_string();
                }
                ProviderError::Network(message) => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_NETWORK_FAILURE".to_string();
                    user_message = format!(
                        "The model provider could not be reached: {message}. The task can be resumed when connectivity returns."
                    );
                }
                ProviderError::Timeout { phase, timeout } => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_TIMEOUT".to_string();
                    user_message = format!(
                        "The model provider stopped responding during {phase} for {timeout:?}. The task can be resumed later."
                    );
                }
                ProviderError::RetryExhausted { attempts, last_error, .. } => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_RETRIES_EXHAUSTED".to_string();
                    user_message = format!(
                        "The model provider did not recover after {attempts} attempts: {last_error}. The task can be resumed later."
                    );
                }
                ProviderError::NotConfigured => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_NOT_CONFIGURED".to_string();
                    user_message = "No configured provider can run the selected model. Open Settings, configure credentials, and assign the model.".to_string();
                }
                ProviderError::CredentialMissing { provider } => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_CREDENTIAL_MISSING".to_string();
                    user_message = format!(
                        "The provider configuration '{provider}' is missing its credential. Open Settings to add it."
                    );
                }
                ProviderError::UnsupportedProvider { provider } => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_UNSUPPORTED".to_string();
                    user_message = format!(
                        "The selected model resolves to unsupported provider type '{provider}'. Update or remove that provider configuration."
                    );
                }
                ProviderError::HttpStatus { status, message, .. } => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_HTTP_STATUS".to_string();
                    user_message = format!("The model provider returned HTTP {status}: {message}");
                }
                ProviderError::Serialization(message) => {
                    audience = FailureAudience::Developer;
                    code = "PROVIDER_SERIALIZATION".to_string();
                    user_message =
                        format!("The model provider response could not be parsed: {message}");
                }
                ProviderError::InvalidResponse(message) => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_INVALID_RESPONSE".to_string();
                    user_message =
                        format!("The model provider returned an unexpected response: {message}");
                }
                ProviderError::Other(message) => {
                    audience = FailureAudience::User;
                    code = "PROVIDER_ERROR".to_string();
                    user_message = format!("The model provider reported an error: {message}");
                } // No `_` arm on purpose (ADR-54): every current variant maps to
                  // a specific code, and adding a new variant must fail
                  // compilation so its classification is decided consciously.
            },
        }

        ClassifiedFailure { audience, code, user_message, dev_details }
    }
}

/// Matches design-stage failures regardless of the registered agent's id:
/// legacy messages still say "architect", tag-driven ones say "design agent".
fn is_architect_structured_output_failure(message: &str) -> bool {
    let lowercase = message.to_ascii_lowercase();
    (lowercase.contains("architect") || lowercase.contains("design agent"))
        && (lowercase.contains("could not be parsed")
            || lowercase.contains("valid designdoc")
            || lowercase.contains("empty designdoc")
            || lowercase.contains("structured output")
            || lowercase.contains("schema validation"))
}

fn is_architect_failure(message: &str) -> bool {
    let lowercase = message.to_ascii_lowercase();
    lowercase.contains("architect failed") || lowercase.contains("design agent failed")
}

/// Matches local memory-storage initialization failures (ADR-54): the
/// runtime wraps `MemoryDb` connect/`app_data_dir` errors as
/// `AgentLoopError`s with these markers.
fn is_memory_init_failure(message: &str) -> bool {
    let lowercase = message.to_ascii_lowercase();
    lowercase.contains("memorydb connect") || lowercase.contains("data directory error")
}

impl ClassifiedFailure {
    /// Helper to create a user-facing failure.
    pub fn user(code: impl Into<String>, message: impl Into<String>) -> Self {
        ClassifiedFailure {
            audience: FailureAudience::User,
            code: code.into(),
            user_message: message.into(),
            dev_details: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentId;

    #[test]
    fn missing_model_capability_is_user_facing() {
        let failure = ClassifiedFailure::from(OrchestratorError::PinnedModelMissingCapability {
            role: AgentId::new("coder"),
            model: "text-only".into(),
            capability: "tool_calling".into(),
        });
        assert_eq!(failure.audience, FailureAudience::User);
        assert_eq!(failure.code, "MODEL_CAPABILITY_MISSING");
        assert!(failure.user_message.contains("tool_calling"));
    }

    #[test]
    fn exhausted_subtask_recovery_is_not_an_internal_error() {
        let failure = ClassifiedFailure::from(OrchestratorError::SubTaskRetriesExhausted {
            task_id: crate::types::TaskId::new(),
            role: AgentId::new("coder"),
            attempts: 3,
            last_error: "filesystem path was not found".into(),
        });
        assert_eq!(failure.audience, FailureAudience::User);
        assert_eq!(failure.code, "SUBTASK_RECOVERY_EXHAUSTED");
        assert!(failure.user_message.contains("filesystem path was not found"));
    }

    #[test]
    fn provider_cancellation_is_reported_as_task_cancellation() {
        let failure =
            ClassifiedFailure::from(OrchestratorError::Provider(ProviderError::Cancelled));
        assert_eq!(failure.audience, FailureAudience::User);
        assert_eq!(failure.code, "TASK_CANCELLED");
        assert_eq!(failure.user_message, "The task was cancelled.");
    }

    #[test]
    fn architect_json_failure_is_not_reported_as_internal_error() {
        let failure = ClassifiedFailure::from(OrchestratorError::AgentLoopError(
            "Architect failed: Architect output could not be parsed after 3 attempts. Last error: DesignDoc schema validation failed".into(),
        ));
        assert_eq!(failure.audience, FailureAudience::User);
        assert_eq!(failure.code, "AGENT_OUTPUT_INVALID");
        assert!(failure.user_message.contains("design agent"));
        assert!(!failure.user_message.contains("internal"));
    }

    #[test]
    fn generic_architect_failure_has_specific_user_message() {
        let failure = ClassifiedFailure::from(OrchestratorError::AgentLoopError(
            "Architect failed: provider returned no result".into(),
        ));
        assert_eq!(failure.audience, FailureAudience::User);
        assert_eq!(failure.code, "ARCHITECT_AGENT_FAILED");
    }

    #[test]
    fn design_agent_failure_is_classified_like_architect_failure() {
        let failure = ClassifiedFailure::from(OrchestratorError::AgentLoopError(
            "Design agent failed: provider returned no result".into(),
        ));
        assert_eq!(failure.audience, FailureAudience::User);
        assert_eq!(failure.code, "ARCHITECT_AGENT_FAILED");
        assert!(failure.user_message.contains("design agent"));
    }

    #[test]
    fn design_agent_empty_design_doc_is_reported_as_invalid_output() {
        let failure = ClassifiedFailure::from(OrchestratorError::AgentLoopError(
            "Design agent produced an empty DesignDoc: no content".into(),
        ));
        assert_eq!(failure.audience, FailureAudience::User);
        assert_eq!(failure.code, "AGENT_OUTPUT_INVALID");
    }

    #[test]
    fn provider_context_overflow_explains_automatic_compaction() {
        let failure =
            ClassifiedFailure::from(OrchestratorError::Provider(ProviderError::ContextOverflow {
                tokens_in: 10_000,
                capacity: 8_000,
            }));
        assert_eq!(failure.code, "PROVIDER_CONTEXT_OVERFLOW");
        assert!(failure.user_message.contains("automatic compaction"));
    }

    #[test]
    fn memory_init_failure_is_classified_as_memory_init_failed() {
        let failure = ClassifiedFailure::from(OrchestratorError::AgentLoopError(
            "MemoryDb connect error: error returned from database: (code: 14) unable to open database file"
                .into(),
        ));
        assert_eq!(failure.audience, FailureAudience::User);
        assert_eq!(failure.code, "MEMORY_INIT_FAILED");
        assert!(failure.user_message.contains("memory"));
    }

    #[test]
    fn generic_agent_loop_error_is_identifying_not_internal() {
        let failure = ClassifiedFailure::from(OrchestratorError::AgentLoopError(
            "an unexpected loop condition".into(),
        ));
        assert_eq!(failure.code, "AGENT_LOOP_FAILED");
        assert!(failure.user_message.contains("an unexpected loop condition"));
        assert_ne!(failure.code, "INTERNAL_ERROR");
    }

    /// Living checklist (ADR-54): every known `OrchestratorError` variant must
    /// classify to a specific code — nothing may fall through to the generic
    /// `INTERNAL_ERROR`. The enum is `#[non_exhaustive]`, so add new variants
    /// here as they appear; the failing assertion tells you which one.
    #[test]
    fn every_orchestrator_error_variant_classifies_with_a_specific_code() {
        use crate::error::{MemoryError, ToolError};
        use crate::types::TaskId;

        let cases: Vec<(&str, OrchestratorError)> = vec![
            (
                "MEMORY_INIT_FAILED",
                OrchestratorError::AgentLoopError(
                    "MemoryDb connect error: unable to open database file".into(),
                ),
            ),
            (
                "AGENT_LOOP_FAILED",
                OrchestratorError::AgentLoopError("generic loop condition".into()),
            ),
            ("MAX_ITERATIONS_REACHED", OrchestratorError::MaxIterationsReached { max: 5 }),
            (
                "CYCLE_DETECTED",
                OrchestratorError::CycleDetected { tool_name: "read_file".into(), count: 3 },
            ),
            ("TASK_CANCELLED", OrchestratorError::Cancelled),
            (
                "UNRECOVERABLE_ERROR",
                OrchestratorError::Unrecoverable { message: "disk full".into() },
            ),
            ("TASK_GRAPH_ERROR", OrchestratorError::TaskGraphError("missing dependency".into())),
            ("TASK_CANCELLED", OrchestratorError::Provider(ProviderError::Cancelled)),
            (
                "TOOL_ERROR",
                OrchestratorError::Tool(ToolError::ExecutionFailed { message: "boom".into() }),
            ),
            ("MEMORY_ERROR", OrchestratorError::Memory(MemoryError::NotFound("chunk".into()))),
            (
                "PLANNING_FAILED",
                OrchestratorError::MultiAgentPlanFailed { reason: "no plan".into() },
            ),
            ("INVALID_TASK_GRAPH", OrchestratorError::InvalidTaskGraph { reason: "cycle".into() }),
            ("BUDGET_EXHAUSTED", OrchestratorError::NoBudgetForDelegation),
            (
                "REVIEW_RECOVERY_EXHAUSTED",
                OrchestratorError::MaxReviewCyclesExceeded { task_id: TaskId::new(), cycles: 3 },
            ),
            (
                "VALIDATION_RECOVERY_EXHAUSTED",
                OrchestratorError::MaxValidationCyclesExceeded {
                    task_id: TaskId::new(),
                    cycles: 3,
                },
            ),
            (
                "NO_AFFORDABLE_MODEL",
                OrchestratorError::NoAffordableModel { role: AgentId::new("coder") },
            ),
            (
                "NO_CAPABLE_MODEL",
                OrchestratorError::NoCapableModel {
                    role: AgentId::new("coder"),
                    capability: "tool_calling".into(),
                },
            ),
            (
                "MODEL_ASSIGNMENT_NOT_FOUND",
                OrchestratorError::PinnedModelNotFound {
                    role: AgentId::new("coder"),
                    provider_config_id: None,
                    model: "gpt-4".into(),
                },
            ),
            (
                "MODEL_CAPABILITY_MISSING",
                OrchestratorError::PinnedModelMissingCapability {
                    role: AgentId::new("coder"),
                    model: "gpt-4".into(),
                    capability: "vision".into(),
                },
            ),
            (
                "MODEL_BUDGET_EXCEEDED",
                OrchestratorError::PinnedModelBudgetExceeded {
                    model: "gpt-4".into(),
                    estimated: 2.0,
                    remaining: 1.0,
                },
            ),
            ("ACTION_REQUIRED_NO_TOOLS", OrchestratorError::ExecutionRequiredButNoTools),
            (
                "SUBTASK_RECOVERY_EXHAUSTED",
                OrchestratorError::SubTaskRetriesExhausted {
                    task_id: TaskId::new(),
                    role: AgentId::new("coder"),
                    attempts: 3,
                    last_error: "timed out".into(),
                },
            ),
            (
                "INVALID_POLICY_CONFIGURATION",
                OrchestratorError::InvalidPolicyConfiguration { reason: "bad rule".into() },
            ),
        ];
        for (expected_code, error) in cases {
            let failure = ClassifiedFailure::from(error);
            assert_eq!(
                failure.code, expected_code,
                "variant must classify to a specific code, not INTERNAL_ERROR (info: {failure:?})"
            );
        }
    }

    /// Every known `ProviderError` variant must classify to a specific code
    /// (ADR-54); the `_` arm stays only for future non-exhaustive additions.
    #[test]
    fn every_provider_error_variant_classifies_with_a_specific_code() {
        let errors: Vec<ProviderError> = vec![
            ProviderError::NotConfigured,
            ProviderError::CredentialMissing { provider: "openai".into() },
            ProviderError::UnsupportedProvider { provider: "foo".into() },
            ProviderError::RateLimit { retry_after: std::time::Duration::from_secs(5) },
            ProviderError::HttpStatus { status: 500, retry_after: None, message: "boom".into() },
            ProviderError::ContextOverflow { tokens_in: 9, capacity: 8 },
            ProviderError::AuthFailure,
            ProviderError::Cancelled,
            ProviderError::Network("dns failed".into()),
            ProviderError::Timeout { phase: "connect", timeout: std::time::Duration::from_secs(1) },
            ProviderError::Serialization("bad json".into()),
            ProviderError::InvalidResponse("malformed".into()),
            ProviderError::Other("vendor hiccup".into()),
            ProviderError::RetryExhausted {
                attempts: 4,
                elapsed: std::time::Duration::from_secs(10),
                last_error: "timeout".into(),
            },
        ];
        for error in errors {
            let failure = ClassifiedFailure::from(OrchestratorError::Provider(error));
            assert_ne!(
                failure.code, "INTERNAL_ERROR",
                "provider variant must classify specifically: {failure:?}"
            );
        }
    }
}
