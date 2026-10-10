//! Specialist tool execution and completion; evaluator integration stays in the parent.

use super::*;
use crate::agents::execution_state::{display_reason, tool_failure_payload, ExecutionState};

impl GenericSpecialistAgent {
    /// Execute through the existing executor, retaining observed progress when
    /// the model stops early or cannot finish within the execution bound.
    ///
    /// Every tool call passes through the shared tool-call guard
    /// ([`guard_coordinator_tool_call`]) before execution, so weak-model
    /// argument defects (e.g. `arguments: null`) are repaired or answered
    /// with a corrective tool result instead of raw executor errors.
    ///
    /// (Private inherent helper — the `ExpertAgent` trait's `run` dispatches
    /// here when `output_mode` is `Freeform`.)
    pub(super) async fn run_freeform(
        &self,
        task: &SubTask,
        context: AgentContext,
        model: &str,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        let agent_id = self.id.as_str();
        let _ = self.bus.publish_for_session(
            task.session_id,
            task.id.0,
            EventKind::AgentThought {
                agent_id: agent_id.to_string(),
                content: format!("Starting {} for task {}", self.name, task.id),
                kind: ThinkingKind::Headline,
            },
        );

        let prompt = self.build_prompt(task, &context).await;
        let tool_defs = self
            .tool_executor
            .as_ref()
            .map(|executor| executor.tool_definitions())
            .unwrap_or_default();

        let start = std::time::Instant::now();
        let mut messages = vec![Message {
            role: Role::User,
            content: prompt.clone(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }];
        // Per-run corrective-retry streaks, mirroring the single-agent loop's
        // `tool_guard_rejects` map: at most
        // [`tool_guard::MAX_TOOL_GUARD_REJECTS`] corrective injections per
        // tool before the exhausted message tells the model to move on.
        let mut tool_guard_rejects: HashMap<String, u32> = HashMap::new();
        // NOTE: chars/4 is a heuristic until provider usage is plumbed through.
        let mut tokens_in = 0_u64;
        let mut tokens_out = 0_u64;
        let mut tool_call_count = 0_u32;
        let mut files_modified = Vec::new();
        let mut summary = String::new();
        let mut progress = ExecutionState::for_task(task, &context);

        // ADR-66 §4: the universal text-fallback driver engages
        // automatically when the provider lacks native tool support (never
        // for plugin providers — those are hard-gated to AnswerOnly tasks
        // with an explicit error). Fallback-driven requests carry no wire
        // tool declarations; the driver's prompt section replaces them.
        // The generic/collaborator path honors the agent's advertised flag: a
        // provider that advertises its absence (`Some(false)`) engages the
        // driver; `None` resolves via the optimistic default, with advertised
        // absence additionally honored earlier by the tool-calling profile
        // filter.
        let mut tool_driver = (!tool_defs.is_empty()
            && crate::tool_driver::fallback_engaged(
                self.provider.provider_name(),
                model,
                self.advertised_tool_support,
            ))
        .then(|| crate::tool_driver::TextToolDriver::new(tool_defs.clone()));
        if let Some(_driver) = tool_driver.as_ref() {
            // The prompt section is injected per request by
            // [`TextToolDriver::augment_request`] (inserting its own System
            // message when none exists), so the wire request never carries
            // tool declarations and the conversation history stays clean.
            let _ = self.bus.publish_for_session(
                task.session_id,
                task.id.0,
                EventKind::AgentThought {
                    agent_id: agent_id.to_string(),
                    content: format!(
                        "tool_driver: fallback engaged (provider '{}', model '{model}')",
                        self.provider.provider_name()
                    ),
                    kind: ThinkingKind::Detail,
                },
            );
            self.record_tool_driver_event(
                task,
                model,
                "engage",
                "fallback",
                "text-fallback tool driver engaged (no native tool support)",
                &cancel,
            )
            .await;
        }

        for iteration in 0..MAX_TOOL_ITERATIONS {
            if cancel.is_cancelled() {
                return Err(OrchestratorError::Cancelled);
            }
            progress.begin_turn();

            let mut request = CompletionRequest {
                model: model.to_string(),
                messages: messages.clone(),
                tools: match &tool_driver {
                    // Fallback-driven requests carry no wire tool
                    // declarations (ADR-66 §4 driver contract).
                    Some(_) => None,
                    None => (!tool_defs.is_empty()).then_some(tool_defs.clone()),
                },
                tool_choice: None,
                temperature: Some(0.7),
                max_tokens: Some(8192),
                stream: false,
            };
            if let Some(driver) = tool_driver.as_ref() {
                driver.augment_request(&mut request);
            }
            // ADR-48 decision 4: provider-reported usage as the source of
            // truth; the byte/4 heuristic is the fallback per dimension.
            let estimated_tokens_in =
                request.messages.iter().map(|message| message.content.len() as u64).sum::<u64>()
                    / 4;

            let (text, reasoning, tool_calls, usage) =
                match self.complete_provider_audited(&request, task, &cancel).await {
                    Ok(turn) => turn,
                    Err(error) => return Err(error),
                };
            // ADR-66 §4: resolve the turn through the text-fallback driver —
            // structured tool-call blocks parsed from the text, bounded
            // repair on malformed blocks, loud failure on bound exhaustion.
            // Native turns (driver inactive) pass through untouched.
            let (text, reasoning, tool_calls, usage) = match tool_driver.as_mut() {
                Some(driver) => {
                    self.freeform_driver_turn(
                        driver,
                        task,
                        model,
                        text,
                        reasoning,
                        usage,
                        &mut messages,
                        &cancel,
                    )
                    .await?
                }
                None => (text, reasoning, tool_calls, usage),
            };
            // ADR-48 decision 4: provider-reported usage as the source of
            // truth; the byte/4 heuristic is the fallback per dimension.
            let usage_in = usage.as_ref().and_then(|u| u.prompt_tokens);
            let usage_out = usage.as_ref().and_then(|u| u.completion_tokens);
            tokens_in = tokens_in.saturating_add(usage_in.unwrap_or(estimated_tokens_in));
            tokens_out = tokens_out.saturating_add(usage_out.unwrap_or((text.len() / 4) as u64));

            messages.push(Message {
                role: Role::Assistant,
                content: text.clone(),
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls.clone()),
                tool_results: None,
                reasoning_content: reasoning,
                tokens_in: usage.as_ref().and_then(|u| u.prompt_tokens),
                tokens_out: usage.as_ref().and_then(|u| u.completion_tokens),
            });

            // Attribute the prompt usage to the preceding user message so the
            // persisted transcript carries measured costs (ADR-48).
            if let (Some(prompt_tokens), Some(user_message)) = (
                usage.as_ref().and_then(|u| u.prompt_tokens),
                messages.iter_mut().rev().find(|m| m.role == Role::User),
            ) {
                user_message.tokens_in = Some(prompt_tokens);
            }

            if tool_calls.is_empty() {
                progress.final_answer();
                summary = text;
                break;
            }

            let Some(executor) = &self.tool_executor else {
                progress.unavailable_executor();
                for call in &tool_calls {
                    progress.failed(
                        &call.id,
                        &call.name,
                        &call.arguments,
                        "specialist-no-executor",
                    );
                }
                summary = text;
                break;
            };

            for tool_call in tool_calls {
                tool_call_count = tool_call_count.saturating_add(1);
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    task.id.0,
                    EventKind::AgentThought {
                        agent_id: agent_id.to_string(),
                        content: tool_execution_description(&tool_call.name, &tool_call.arguments),
                        kind: ThinkingKind::Detail,
                    },
                );
                // Tool-call guard (VALIDATE → COERCE → INFER → EXTRACT →
                // REPAIR): normalize the provider-accumulated arguments
                // before execution. `text` is the assistant message that
                // carried these tool calls — its intent feeds the guard's
                // text-extraction backstop. Rejected calls never execute;
                // the model receives a corrective tool result and retries on
                // the next iteration.
                let arguments = match guard_coordinator_tool_call(
                    &tool_call.name,
                    &tool_call.arguments,
                    executor,
                    &mut tool_guard_rejects,
                    Some(text.as_str()),
                ) {
                    GuardedArguments::Pass(arguments) => arguments,
                    GuardedArguments::Reject { content, payload } => {
                        progress.rejected(&tool_call.id, &tool_call.name, &tool_call.arguments);
                        let _ = self.bus.publish_for_session(
                            task.session_id,
                            task.id.0,
                            EventKind::AgentThought {
                                agent_id: agent_id.to_string(),
                                content: content.clone(),
                                kind: ThinkingKind::Detail,
                            },
                        );
                        messages.push(Message {
                            role: Role::Tool,
                            content,
                            tool_calls: None,
                            tool_results: Some(vec![ToolResult {
                                id: tool_call.id,
                                name: tool_call.name.clone(),
                                content: payload,
                            }]),
                            reasoning_content: None,
                            tokens_in: None,
                            tokens_out: None,
                        });
                        continue;
                    }
                };
                // ADR-82 slice 1: resolve the canonical policy-view identity
                // once per call (the backend owns the registry; a backend
                // without one yields None and the legacy grammar applies).
                // The write classification reads the canonical identity so a
                // `write` alias classifies as a filesystem write, while still
                // reading the guarded arguments.
                let canonical = executor.canonical_effect(&tool_call.name, &arguments);
                let canonical_tool = canonical.as_ref().map(|effect| effect.policy_name.clone());
                let canonical_operation =
                    canonical.as_ref().and_then(|effect| effect.operation.clone());
                let is_file_change = match &canonical {
                    Some(effect) => crate::tool_facts::is_file_affecting_tool(
                        &effect.policy_name,
                        effect.operation.as_deref(),
                    ),
                    None => crate::tool_facts::is_file_affecting_tool_legacy(
                        &tool_call.name,
                        &arguments,
                    ),
                };
                // ADR-65 §3: hash the pre-write state of every path this
                // command will touch before it runs (fail-soft).
                let pre_image_hashes = match &self.tool_facts {
                    Some(facts) => {
                        let affected = crate::tool_facts::extract_affected_paths(&arguments, None);
                        facts
                            .pre_image_hashes(&context.session.project_dir, &affected, &cancel)
                            .await
                    }
                    None => HashMap::new(),
                };
                // ADR-65 §4: safe read dedupe — a plain single-path filesystem
                // read whose clean observation still matches the disk (re-statted
                // now, content hash verified) is a serve candidate. Any doubt
                // degrades to normal execution; the model receives a
                // byte-identical read result either way.
                let serve = match &self.tool_facts {
                    Some(facts) => {
                        crate::read_cache::maybe_serve_read(
                            facts,
                            &context.session.project_dir,
                            &tool_call.name,
                            &arguments,
                            &cancel,
                        )
                        .await
                    }
                    None => None,
                };
                // ADR-65 F1a: serve only when the policy engine explicitly
                // allows the read through the advisory path (no decision row, no
                // quota consumption); any non-Allow verdict runs the normal,
                // fully policy-checked executor path below.
                let serve = match serve {
                    Some(serve)
                        if executor
                            .policy_verdict_is_allow(
                                &tool_call.name,
                                &arguments,
                                &context.session,
                                cancel.clone(),
                            )
                            .await =>
                    {
                        Some(serve)
                    }
                    _ => None,
                };
                if let Some(serve) = serve {
                    // ADR-65 F1b: the serve consumed no executor decision row —
                    // persist its own ServedFromCache audit row (fail-soft).
                    executor
                        .record_served_read_audit(
                            &tool_call.name,
                            &arguments,
                            &serve.path,
                            &context.session,
                            cancel.clone(),
                        )
                        .await;
                    let served_summary =
                        format!("Read {} bytes from {}", serve.content.len(), serve.path);
                    let output = ToolOutput {
                        summary: served_summary.clone(),
                        data: serde_json::json!({ "content": serve.content, "path": serve.path }),
                    };
                    self.record_served_read_fact(
                        task,
                        &context,
                        &tool_call.name,
                        &arguments,
                        &serve.event_id,
                        canonical_tool.as_deref(),
                        canonical_operation.as_deref(),
                        &cancel,
                    )
                    .await;
                    progress.succeeded(&tool_call.id, &tool_call.name, &arguments);
                    messages.push(Message {
                        role: Role::Tool,
                        content: String::new(),
                        tool_calls: None,
                        tool_results: Some(vec![ToolResult {
                            id: tool_call.id,
                            name: tool_call.name.clone(),
                            content: serde_json::to_value(&output).unwrap_or_default(),
                        }]),
                        reasoning_content: None,
                        tokens_in: None,
                        tokens_out: None,
                    });
                    continue;
                }
                match executor
                    .execute(&tool_call.name, arguments.clone(), &context.session, cancel.clone())
                    .await
                {
                    Ok(output) => {
                        progress.succeeded(&tool_call.id, &tool_call.name, &arguments);
                        if is_file_change {
                            // Prefer the destination for move/copy (the file
                            // actually created); read/write/list report "path".
                            if let Some(path) = output
                                .data
                                .get("destination")
                                .or_else(|| output.data.get("path"))
                                .or_else(|| output.data.get("file_path"))
                                .and_then(|value| value.as_str())
                                .or_else(|| {
                                    tool_call.arguments.get("path").and_then(|value| value.as_str())
                                })
                            {
                                let path = camino::Utf8PathBuf::from(path);
                                if !files_modified.contains(&path) {
                                    files_modified.push(path);
                                }
                            }
                        }
                        // ADR-65 §3: record the completed (successful) tool
                        // command with the paths it actually touched.
                        self.record_tool_fact(
                            task,
                            &context,
                            &tool_call.name,
                            &arguments,
                            true,
                            ToolOutcome::Ok,
                            Some(&output.data),
                            is_file_change,
                            pre_image_hashes.clone(),
                            canonical_tool.as_deref(),
                            canonical_operation.as_deref(),
                            &cancel,
                        )
                        .await;
                        // ADR-65 §4: cache the exact bytes of a successful plain
                        // read (after the observation above, so the row exists)
                        // so an identical later read can be served. Fail-soft.
                        if let Some(facts) = &self.tool_facts {
                            crate::read_cache::cache_read_output(
                                facts,
                                &context.session.project_dir,
                                &tool_call.name,
                                &arguments,
                                &output.data,
                                &cancel,
                            )
                            .await;
                        }
                        messages.push(Message {
                            role: Role::Tool,
                            content: String::new(),
                            tool_calls: None,
                            tool_results: Some(vec![ToolResult {
                                id: tool_call.id,
                                name: tool_call.name.clone(),
                                content: serde_json::to_value(&output).unwrap_or_default(),
                            }]),
                            reasoning_content: None,
                            tokens_in: None,
                            tokens_out: None,
                        });
                    }
                    Err(error) => {
                        // ADR-65 §3: record the completed (failed) tool
                        // command too — evidence exists either way.
                        self.record_tool_fact(
                            task,
                            &context,
                            &tool_call.name,
                            &arguments,
                            false,
                            tool_outcome_for_error(&error),
                            None,
                            is_file_change,
                            pre_image_hashes.clone(),
                            canonical_tool.as_deref(),
                            canonical_operation.as_deref(),
                            &cancel,
                        )
                        .await;
                        if cancel.is_cancelled()
                            || matches!(error, concerto_core::ToolError::Cancelled)
                        {
                            return Err(OrchestratorError::Cancelled);
                        }
                        if matches!(error, concerto_core::ToolError::PausedAwaitingApproval { .. })
                        {
                            return Err(OrchestratorError::Tool(error));
                        }
                        let payload = tool_failure_payload(&error);
                        let code = payload
                            .get("code")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("tool-failed");
                        progress.failed(&tool_call.id, &tool_call.name, &arguments, code);
                        let _ = self.bus.publish_for_session(
                            task.session_id,
                            task.id.0,
                            EventKind::AgentThought {
                                agent_id: agent_id.to_string(),
                                content: format!(
                                    "Tool {} failed: {error}. Returning the error to the model ({}/{}).",
                                    tool_call.name,
                                    iteration + 1,
                                    MAX_TOOL_ITERATIONS
                                ),
                                kind: ThinkingKind::Detail,
                            },
                        );
                        messages.push(Message {
                            role: Role::Tool,
                            content: String::new(),
                            tool_calls: None,
                            tool_results: Some(vec![ToolResult {
                                id: tool_call.id,
                                name: tool_call.name.clone(),
                                content: payload,
                            }]),
                            reasoning_content: None,
                            tokens_in: None,
                            tokens_out: None,
                        });
                    }
                }
            }

            // Preserve the last text as feedback. A turn still requesting
            // tools is unfinished; the bound never manufactures success.
            if iteration + 1 >= MAX_TOOL_ITERATIONS {
                summary = text;
            }
        }

        let latency_ms = start.elapsed().as_millis() as u64;
        let cost_usd = self.provider.approximate_cost(tokens_in, tokens_out);
        let outcome = progress.outcome(task, &summary);
        if let AgentOutcome::NeedsRevision { reason } = &outcome {
            let detail = display_reason(reason);
            summary = if summary.trim().is_empty() {
                format!("Needs continuation: {detail}")
            } else {
                format!("Needs continuation: {detail}\nLast model message: {summary}")
            };
        }
        let settlement = if matches!(outcome, AgentOutcome::Success) {
            "finished"
        } else {
            "needs continuation"
        };

        let _ = self.bus.publish_for_session(
            task.session_id,
            task.id.0,
            EventKind::AgentThought {
                agent_id: agent_id.to_string(),
                content: format!("{} {settlement} ({tokens_in} in, {tokens_out} out)", self.name),
                kind: ThinkingKind::Headline,
            },
        );

        Ok(AgentRunResult {
            task_id: task.id,
            role: self.id.clone(),
            outcome,
            summary,
            files_modified,
            tool_call_count,
            cost_usd,
            latency_ms,
            provider: self.provider.provider_name().to_string(),
            model: model.to_string(),
            tokens_in,
            tokens_out,
        })
    }
}
