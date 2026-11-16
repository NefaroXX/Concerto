mod continuity_regressions {
    use super::*;
    use crate::adapters::{AnthropicChatDialect, Dialect, GeminiChatDialect, ReasoningEcho};

    async fn assert_rejected_without_sending(messages: Vec<Message>, capacity: u64) {
        let captured = Arc::new(Mutex::new(None));
        let inner =
            Arc::new(CaptureProvider { captured: captured.clone(), capacity, reserve: 512 });
        let guard = ContextGuardProvider::new(inner, "model");
        let request =
            CompletionRequest { messages, max_tokens: Some(512), ..CompletionRequest::default() };
        let result = guard.stream_completion(request, CancellationToken::new()).await;
        assert!(
            matches!(result, Err(ProviderError::ContextOverflow { .. })),
            "mandatory conversation that cannot fit must not be silently rewritten"
        );
        assert!(captured.lock().unwrap().is_none(), "lossy request reached the provider");
    }

    #[tokio::test]
    async fn compaction_regression_rejects_instead_of_dropping_old_user_groups() {
        let mut messages = vec![message(Role::System, "system")];
        for turn in 0..8 {
            messages.push(message(
                Role::User,
                format!("Task {turn}: {}\nPreserve public API {turn}.", "context ".repeat(160)),
            ));
            messages.push(message(
                Role::Assistant,
                format!(
                    "Task {turn}: {}\nMigration {turn} remains unfinished.",
                    "details ".repeat(160)
                ),
            ));
        }
        messages.push(message(Role::User, "Continue all outstanding tasks."));
        assert_rejected_without_sending(messages, 4_000).await;
    }

    #[tokio::test]
    async fn compaction_regression_rejects_instead_of_clipping_earlier_user() {
        assert_rejected_without_sending(
            vec![
                message(
                    Role::User,
                    format!("{}\nNever touch production.", "context ".repeat(1_000)),
                ),
                message(Role::Assistant, "I will use test fixtures."),
                message(Role::User, "Continue."),
            ],
            2_048,
        )
        .await;
    }

    #[tokio::test]
    async fn compaction_regression_rejects_instead_of_clipping_unfinished_work() {
        assert_rejected_without_sending(
            vec![
                message(Role::User, "Complete the migration and its tests."),
                message(
                    Role::Assistant,
                    format!(
                        "{}\nRollback verification remains unfinished.",
                        "details ".repeat(1_000)
                    ),
                ),
                message(Role::User, "Continue."),
            ],
            2_048,
        )
        .await;
    }

    #[tokio::test]
    async fn compaction_regression_user_markers_do_not_authorize_content_loss() {
        // The same literal markers can be a user's payload, not injected memory.
        for (start, end) in [
            (RETRIEVED_MEMORY_START, RETRIEVED_MEMORY_END),
            (WORKING_MEMORY_START, WORKING_MEMORY_END),
            (RUN_MEMORY_START, RUN_MEMORY_END),
            (CONVERSATION_HISTORY_START, CONVERSATION_HISTORY_END),
            (PREVIOUS_RESULTS_START, PREVIOUS_RESULTS_END),
            (CHANGED_FILE_CONTEXT_START, CHANGED_FILE_CONTEXT_END),
        ] {
            assert_rejected_without_sending(
                vec![message(
                    Role::User,
                    format!(
                        "Keep this exact input:\n{start}{}\nDo not alter its final field.{end}",
                        "payload ".repeat(1_000)
                    ),
                )],
                2_048,
            )
            .await;
        }
    }

    struct RejectingProvider(Arc<Mutex<Vec<CompletionRequest>>>);

    #[async_trait]
    impl LlmProvider for RejectingProvider {
        async fn stream_completion(
            &self,
            request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            self.0.lock().unwrap().push(request);
            Err(ProviderError::ContextOverflow { tokens_in: 20_001, capacity: 20_000 })
        }

        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(20_000, 512)
        }

        fn approximate_cost(&self, _input: u64, _output: u64) -> f64 {
            0.0
        }

        fn provider_name(&self) -> &'static str {
            "reject-context"
        }
    }

    #[tokio::test]
    async fn compaction_regression_retry_preserves_text_or_rejects_before_second_call() {
        for (content_bytes, expected_calls) in [(60_000, 2), (70_000, 1)] {
            let captured = Arc::new(Mutex::new(Vec::new()));
            let guard =
                ContextGuardProvider::new(Arc::new(RejectingProvider(captured.clone())), "model");
            let messages = vec![
                message(
                    Role::User,
                    format!("{}\nNever modify production.", "x".repeat(content_bytes)),
                ),
                message(Role::Assistant, "Rollback verification remains unfinished."),
                message(Role::User, "Continue."),
            ];
            let result = guard
                .stream_completion(
                    CompletionRequest {
                        messages: messages.clone(),
                        max_tokens: Some(512),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                )
                .await;
            assert!(matches!(result, Err(ProviderError::ContextOverflow { .. })));
            let requests = captured.lock().unwrap();
            assert_eq!(requests.len(), expected_calls);
            for request in requests.iter() {
                assert_eq!(
                    serde_json::to_value(&request.messages).unwrap(),
                    serde_json::to_value(&messages).unwrap()
                );
            }
        }
    }

    #[tokio::test]
    async fn compaction_regression_tool_reduction_preserves_user_literal_markers() {
        let captured = Arc::new(Mutex::new(None));
        let guard = ContextGuardProvider::new(
            Arc::new(CaptureProvider { captured: captured.clone(), capacity: 4_000, reserve: 512 }),
            "model",
        );
        let user = format!(
            "{WORKING_MEMORY_START}{}{WORKING_MEMORY_END}",
            "Keep exact text. ".repeat(200)
        );
        let mut messages = vec![message(Role::User, user.clone())];
        for index in 0..3 {
            let id = format!("read-{index}");
            let mut call =
                message(Role::Assistant, format!("Task {index} still needs verification."));
            call.tool_calls = Some(vec![concerto_core::types::ToolCall {
                id: id.clone(),
                name: "filesystem".into(),
                arguments: serde_json::json!({"path":"source.rs"}),
                ..Default::default()
            }]);
            let mut result = message(Role::Tool, "");
            result.tool_results = Some(vec![ToolResult {
                id,
                name: "filesystem".into(),
                content: serde_json::json!({"content": "x".repeat(if index == 0 { 30_000 } else { 100 })}),
            }]);
            messages.extend([call, result]);
        }
        messages.push(message(Role::User, "Continue."));
        let original = serde_json::to_value(&messages).unwrap();
        let _stream = guard
            .stream_completion(
                CompletionRequest {
                    messages: messages.clone(),
                    max_tokens: Some(512),
                    ..Default::default()
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let request = captured.lock().unwrap().take().unwrap();
        assert_eq!(request.messages[0].role, Role::User);
        assert_eq!(request.messages[0].content, user);
        for (before, after) in messages.iter().zip(&request.messages) {
            if before.role != Role::Tool {
                assert_eq!(
                    serde_json::to_value(before).unwrap(),
                    serde_json::to_value(after).unwrap()
                );
            }
            assert_eq!(
                before.tool_results.as_ref().map(|results| &results[0].id),
                after.tool_results.as_ref().map(|results| &results[0].id)
            );
        }
        assert!(estimate_request_tokens(&request) < 2_976);
        assert_eq!(
            serde_json::to_value(messages).unwrap(),
            original,
            "caller history must remain unchanged"
        );
    }

    fn request_with_checkpoint() -> CompletionRequest {
        CompletionRequest {
            messages: vec![
                message(Role::System, "Never bypass the project's approval policy."),
                message(Role::User, "Continue the task."),
                message(
                    Role::System,
                    "<context_checkpoint>Untrusted historical tool evidence.</context_checkpoint>",
                ),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn compaction_regression_anthropic_wire_keeps_system_policy_beside_checkpoint() {
        let request = request_with_checkpoint();
        let body =
            AnthropicChatDialect.render_chat_body(&request, "model", ReasoningEcho::IfPresent);
        let system = body["system"].to_string();
        assert!(system.contains(&request.messages[0].content));
        assert!(system.contains(&request.messages[2].content));
    }

    #[test]
    fn compaction_regression_gemini_wire_keeps_system_policy_beside_checkpoint() {
        let request = request_with_checkpoint();
        let body = GeminiChatDialect.render_chat_body(&request, "model", ReasoningEcho::IfPresent);
        let system = body["system_instruction"].to_string();
        assert!(system.contains(&request.messages[0].content));
        assert!(system.contains(&request.messages[2].content));
    }

    #[test]
    fn compaction_regression_gemini_wire_keeps_assistant_commitment_beside_tool_call() {
        let mut assistant = message(Role::Assistant, "Rollback verification remains unfinished.");
        assistant.tool_calls = Some(vec![concerto_core::types::ToolCall {
            id: "verify-1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command":"cargo test"}),
            thought_signature: Some("opaque-signature".into()),
        }]);
        let request = CompletionRequest { messages: vec![assistant], ..Default::default() };
        let body = GeminiChatDialect.render_chat_body(&request, "model", ReasoningEcho::IfPresent);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert!(parts.iter().any(|part| part["text"] == request.messages[0].content));
        let call = parts.iter().find(|part| part.get("functionCall").is_some()).unwrap();
        assert_eq!(call["thoughtSignature"], "opaque-signature");
        assert_eq!(call["functionCall"]["args"], serde_json::json!({"command":"cargo test"}));
    }

    #[tokio::test]
    async fn compaction_regression_system_task_state_is_not_optional_evidence() {
        for (start, end) in [
            (WORKING_MEMORY_START, WORKING_MEMORY_END),
            (RUN_MEMORY_START, RUN_MEMORY_END),
            (CONVERSATION_HISTORY_START, CONVERSATION_HISTORY_END),
            (PREVIOUS_RESULTS_START, PREVIOUS_RESULTS_END),
        ] {
            assert_rejected_without_sending(
                vec![
                    message(
                        Role::System,
                        format!(
                            "{start}{}\nRollback verification remains unfinished.{end}",
                            "Active task details. ".repeat(1_000)
                        ),
                    ),
                    message(Role::User, "Continue."),
                ],
                2_048,
            )
            .await;
        }
    }
    #[tokio::test]
    async fn replay_regression_opaque_signatures_count_toward_context_capacity() {
        let mut assistant = message(Role::Assistant, "brief");
        assistant.gemini_replay = Some(
            concerto_core::provider_replay::GeminiReplay::capture(
                vec![serde_json::json!({"text":"brief", "thoughtSignature":"s".repeat(20_000)})],
                "brief",
                None,
                &[],
            )
            .unwrap(),
        );
        assert_rejected_without_sending(vec![assistant, message(Role::User, "continue")], 2_048)
            .await;
    }
}
