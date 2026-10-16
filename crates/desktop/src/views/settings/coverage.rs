//! Additive "Advanced" coverage of previously config-file-only knobs.
//!
//! Part 3 of the Settings coverage work: a single collapsible section plus two
//! update arms (`AdvancedFieldChanged` / `AdvancedToggled`, keyed by
//! [`AdvancedField`] / [`AdvancedToggle`]) expose the remaining operator dials
//! that used to require hand-editing the config file — session spend cap,
//! `git_auto_init`, the single-agent verification gate, the recovery timeouts,
//! the policy auto-approval time window, memory retention, and the
//! audit / observability / updates / context-budget / harness sections.
//!
//! Every field is seeded by `State::from_config` and written back by
//! `State::to_config`; a blank text field preserves the base/default value
//! rather than forcing one. Optional sections (`[audit]`, `[observability]`,
//! `[updates]`, `[context]`, `[harness]`) are published as a whole on save,
//! carrying their pre-existing non-edited keys (secret keys, service name,
//! declared capabilities) forward unchanged.

use iced::widget::{checkbox, column, text, text_input};
use iced::{Element, Length};

use crate::theme::AppTheme;
use crate::ui::{form_field, labeled_slider, SPACING_SM};

use super::{AdvancedField, AdvancedToggle, Message, State};

/// One labeled text field in the Advanced section. `value` is the live edit
/// buffer; `field` selects which state field the edit targets.
fn advanced_text<'a>(
    theme: &'a AppTheme,
    label: &'a str,
    help: &'a str,
    placeholder: &'a str,
    value: &'a str,
    field: AdvancedField,
) -> Element<'a, Message> {
    form_field(
        theme,
        label,
        false,
        Some(help),
        None::<&str>,
        text_input(placeholder, value)
            .on_input(move |next| Message::AdvancedFieldChanged(field, next))
            .width(Length::Fill),
    )
}

/// One boolean toggle row in the Advanced section.
fn advanced_toggle<'a>(
    label: &'a str,
    value: bool,
    toggle: AdvancedToggle,
) -> Element<'a, Message> {
    checkbox(value)
        .label(label)
        .on_toggle(move |next| Message::AdvancedToggled(toggle, next))
        .into()
}

impl State {
    /// Handle the Advanced coverage `Message` group. The parent `State::update`
    /// routes the four `Advanced*` variants here; every arm arms the dirty flag
    /// and a non-Advanced message (never routed by the parent) is a no-op.
    pub(super) fn update_coverage(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::AdvancedFieldChanged(field, value) => {
                self.settings_dirty = true;
                match field {
                    AdvancedField::SessionSpendCapUsd => self.session_spend_cap_usd = value,
                    AdvancedField::RetryTimeToFirstByteSeconds => {
                        self.retry_time_to_first_byte_seconds = value;
                    }
                    AdvancedField::RetryStreamIdleTimeoutSeconds => {
                        self.retry_stream_idle_timeout_seconds = value;
                    }
                    AdvancedField::RetryMaxAttempts => self.retry_max_attempts = value,
                    AdvancedField::PolicyTimeWindowTimezone => {
                        self.policy_time_window_timezone = value;
                    }
                    AdvancedField::PolicyTimeWindowAutoApproveBelowUsd => {
                        self.policy_time_window_auto_approve_below_usd = value;
                    }
                    AdvancedField::MemoryExcludePatterns => self.memory_exclude_patterns = value,
                    AdvancedField::MemorySummaryKeepPerSession => {
                        self.memory_summary_keep_per_session = value;
                    }
                    AdvancedField::MemorySummaryRetentionDays => {
                        self.memory_summary_retention_days = value;
                    }
                    AdvancedField::SkillsMaxChars => {
                        self.skills_max_chars_input = value;
                        self.skills_max_chars = self.skills_max_chars_input.trim().parse().ok();
                    }
                    AdvancedField::ProjectContextMaxBytes => {
                        self.project_context_max_bytes_input = value;
                        self.project_context_max_bytes =
                            self.project_context_max_bytes_input.trim().parse().ok();
                        // The project-context section is only published after an
                        // explicit edit here (`to_config`); arm that gate.
                        self.project_context_dirty = true;
                    }
                    AdvancedField::ProjectContextUpdateFrequency => {
                        self.project_context_update_frequency_input = value;
                        if let Ok(parsed) =
                            self.project_context_update_frequency_input.trim().parse()
                        {
                            self.project_context_update_frequency = parsed;
                        }
                        self.project_context_dirty = true;
                    }
                    AdvancedField::AuditRetentionDays => self.audit_retention_days = value,
                    AdvancedField::AuditArchiveDir => self.audit_archive_dir = value,
                    AdvancedField::ObservabilityPrometheusPort => {
                        self.observability_prometheus_port = value;
                    }
                    AdvancedField::ObservabilityOtlpEndpoint => {
                        self.observability_otlp_endpoint = value;
                    }
                    AdvancedField::ObservabilityLangfuseHost => {
                        self.observability_langfuse_host = value;
                    }
                    AdvancedField::UpdatesUpdateEndpoint => self.updates_update_endpoint = value,
                    AdvancedField::ContextTriggerTokens => self.context_trigger_tokens = value,
                    AdvancedField::ContextRetainUserTurns => self.context_retain_user_turns = value,
                    AdvancedField::ContextMinimumUserTurns => {
                        self.context_minimum_user_turns = value;
                    }
                    AdvancedField::HarnessCheckpointRetentionDays => {
                        self.harness_checkpoint_retention_days = value;
                    }
                    AdvancedField::HarnessRecallLimit => self.harness_recall_limit = value,
                    AdvancedField::HarnessVerifySelector => self.harness_verify_selector = value,
                }
            }
            Message::AdvancedToggled(toggle, value) => {
                self.settings_dirty = true;
                match toggle {
                    AdvancedToggle::GitAutoInit => self.git_auto_init = value,
                    AdvancedToggle::StrictVerificationGate => self.strict_verification_gate = value,
                    AdvancedToggle::PolicyTimeWindowEnabled => {
                        self.policy_time_window_enabled = value;
                    }
                    AdvancedToggle::AuditEncryptAtRest => self.audit_encrypt_at_rest = value,
                    AdvancedToggle::UpdatesCheckOnStartup => self.updates_check_on_startup = value,
                    AdvancedToggle::ContextCacheStablePrefix => {
                        self.context_cache_stable_prefix = value;
                    }
                }
            }
            Message::AdvancedPolicyTimeWindowStartHour(hour) => {
                self.settings_dirty = true;
                self.policy_time_window_start_hour = hour.min(23);
            }
            Message::AdvancedPolicyTimeWindowEndHour(hour) => {
                self.settings_dirty = true;
                self.policy_time_window_end_hour = hour.min(23);
            }
            _ => {}
        }
        iced::Task::none()
    }

    /// Content of the Advanced coverage section. The caller (`view`) wraps this
    /// in the `collapsible_section` card.
    pub(super) fn advanced_section<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        // ── Spend ──
        let spend = advanced_text(
            theme,
            "Session spend cap (USD)",
            "Blank = no cap. Stops the session once estimated spend reaches the cap.",
            "blank = no cap",
            self.session_spend_cap_usd.as_str(),
            AdvancedField::SessionSpendCapUsd,
        );

        // ── Tooling / agent gate ──
        let tooling = column![
            advanced_toggle(
                "Run a bare `git init` for non-repository project directories",
                self.git_auto_init,
                AdvancedToggle::GitAutoInit,
            ),
            advanced_toggle(
                "Strict verification gate (refuse Done without a passed verification)",
                self.strict_verification_gate,
                AdvancedToggle::StrictVerificationGate,
            ),
        ]
        .spacing(SPACING_SM);

        // ── Recovery timeouts ──
        let recovery = column![
            advanced_text(
                theme,
                "First-byte deadline (seconds)",
                "Deadline for receiving response headers / the response stream.",
                "blank = default",
                self.retry_time_to_first_byte_seconds.as_str(),
                AdvancedField::RetryTimeToFirstByteSeconds,
            ),
            advanced_text(
                theme,
                "Stream idle timeout (seconds)",
                "Maximum quiet period between response chunks (reset on each chunk).",
                "blank = default",
                self.retry_stream_idle_timeout_seconds.as_str(),
                AdvancedField::RetryStreamIdleTimeoutSeconds,
            ),
            advanced_text(
                theme,
                "Max attempts",
                "Maximum attempts for one logical provider request, including the first.",
                "blank = default",
                self.retry_max_attempts.as_str(),
                AdvancedField::RetryMaxAttempts,
            ),
        ]
        .spacing(SPACING_SM);

        // ── Policy time window ──
        let mut window = column![advanced_toggle(
            "Enable the auto-approval time window",
            self.policy_time_window_enabled,
            AdvancedToggle::PolicyTimeWindowEnabled,
        )]
        .spacing(SPACING_SM);
        if self.policy_time_window_enabled {
            window = window
                .push(labeled_slider(
                    theme,
                    "Start hour",
                    f32::from(self.policy_time_window_start_hour),
                    0.0..=23.0,
                    |v| Message::AdvancedPolicyTimeWindowStartHour(v.round() as u8),
                    |v| format!("{:.0}:00", v),
                ))
                .push(labeled_slider(
                    theme,
                    "End hour",
                    f32::from(self.policy_time_window_end_hour),
                    0.0..=23.0,
                    |v| Message::AdvancedPolicyTimeWindowEndHour(v.round() as u8),
                    |v| format!("{:.0}:00", v),
                ))
                .push(advanced_text(
                    theme,
                    "Timezone",
                    "IANA name, e.g. America/New_York.",
                    "UTC",
                    self.policy_time_window_timezone.as_str(),
                    AdvancedField::PolicyTimeWindowTimezone,
                ))
                .push(advanced_text(
                    theme,
                    "Auto-approve below (USD)",
                    "Operations below this estimated cost are auto-approved outside the window.",
                    "0.0",
                    self.policy_time_window_auto_approve_below_usd.as_str(),
                    AdvancedField::PolicyTimeWindowAutoApproveBelowUsd,
                ));
        }

        // ── Memory retention ──
        let memory = column![
            advanced_text(
                theme,
                "Exclude patterns",
                "Project-relative globs excluded from background indexing, one per line.",
                "one glob per line",
                self.memory_exclude_patterns.as_str(),
                AdvancedField::MemoryExcludePatterns,
            ),
            advanced_text(
                theme,
                "Summary keep per session",
                "Newest derived summaries kept per session bucket (0 = keep all).",
                "blank = default",
                self.memory_summary_keep_per_session.as_str(),
                AdvancedField::MemorySummaryKeepPerSession,
            ),
            advanced_text(
                theme,
                "Summary retention (days)",
                "Age-based prune window for derived summaries (0 = disabled).",
                "blank = default",
                self.memory_summary_retention_days.as_str(),
                AdvancedField::MemorySummaryRetentionDays,
            ),
        ]
        .spacing(SPACING_SM);

        // ── Skills / project context (previously display-only) ──
        let extensions = column![
            advanced_text(
                theme,
                "Skills prompt budget (chars)",
                "Per-skill prompt budget; blank = crate default.",
                "blank = default",
                self.skills_max_chars_input.as_str(),
                AdvancedField::SkillsMaxChars,
            ),
            advanced_text(
                theme,
                "Project context budget (chars per file)",
                "Per-source character budget for AGENTS.md injection; blank = default (32 KiB).",
                "blank = default",
                self.project_context_max_bytes_input.as_str(),
                AdvancedField::ProjectContextMaxBytes,
            ),
            advanced_text(
                theme,
                "Project context nudge cadence (dispatches)",
                "How often the coordinator is reminded to refresh AGENTS.md.",
                "1",
                self.project_context_update_frequency_input.as_str(),
                AdvancedField::ProjectContextUpdateFrequency,
            ),
        ]
        .spacing(SPACING_SM);

        // ── Audit ──
        let audit = column![
            advanced_toggle(
                "Encrypt the sessions/audit database at rest (SQLCipher)",
                self.audit_encrypt_at_rest,
                AdvancedToggle::AuditEncryptAtRest,
            ),
            advanced_text(
                theme,
                "Audit retention (days)",
                "Rows older than this are archived then deleted; blank = keep forever.",
                "blank = keep forever",
                self.audit_retention_days.as_str(),
                AdvancedField::AuditRetentionDays,
            ),
            advanced_text(
                theme,
                "Audit archive directory",
                "Directory holding the encrypted archive; blank = Concerto data directory.",
                "blank = data directory",
                self.audit_archive_dir.as_str(),
                AdvancedField::AuditArchiveDir,
            ),
        ]
        .spacing(SPACING_SM);

        // ── Observability ──
        let observability = column![
            advanced_text(
                theme,
                "Prometheus port",
                "Metrics export port; blank = Prometheus export disabled.",
                "blank = disabled",
                self.observability_prometheus_port.as_str(),
                AdvancedField::ObservabilityPrometheusPort,
            ),
            advanced_text(
                theme,
                "OTLP endpoint",
                "OTLP HTTP endpoint, e.g. http://localhost:4318; blank = disabled.",
                "blank = disabled",
                self.observability_otlp_endpoint.as_str(),
                AdvancedField::ObservabilityOtlpEndpoint,
            ),
            advanced_text(
                theme,
                "Langfuse host",
                "Langfuse host URL; blank = Langfuse disabled (keys stay in config/keyring).",
                "blank = disabled",
                self.observability_langfuse_host.as_str(),
                AdvancedField::ObservabilityLangfuseHost,
            ),
        ]
        .spacing(SPACING_SM);

        // ── Updates ──
        let updates = column![
            advanced_toggle(
                "Check for updates on startup",
                self.updates_check_on_startup,
                AdvancedToggle::UpdatesCheckOnStartup,
            ),
            advanced_text(
                theme,
                "Update endpoint",
                "Custom update endpoint URL; blank = crates.io API.",
                "blank = crates.io",
                self.updates_update_endpoint.as_str(),
                AdvancedField::UpdatesUpdateEndpoint,
            ),
        ]
        .spacing(SPACING_SM);

        // ── Context budget ──
        let context = column![
            advanced_text(
                theme,
                "Compaction trigger (tokens)",
                "Token budget that triggers deterministic compaction; blank = default (16000).",
                "blank = default",
                self.context_trigger_tokens.as_str(),
                AdvancedField::ContextTriggerTokens,
            ),
            advanced_text(
                theme,
                "Retain recent user turns",
                "Most-recent user turns kept verbatim after compaction; blank = default (4).",
                "blank = default",
                self.context_retain_user_turns.as_str(),
                AdvancedField::ContextRetainUserTurns,
            ),
            advanced_text(
                theme,
                "Minimum user turns before compaction",
                "Compaction cannot fire below this many user turns; blank = default (6).",
                "blank = default",
                self.context_minimum_user_turns.as_str(),
                AdvancedField::ContextMinimumUserTurns,
            ),
            advanced_toggle(
                "Pin a byte-stable prompt prefix (cache-stable assembly)",
                self.context_cache_stable_prefix,
                AdvancedToggle::ContextCacheStablePrefix,
            ),
        ]
        .spacing(SPACING_SM);

        // ── Harness ──
        let harness = column![
            advanced_text(
                theme,
                "Checkpoint retention (days)",
                "Window `harness checkpoint prune` applies; blank = keep forever.",
                "blank = keep forever",
                self.harness_checkpoint_retention_days.as_str(),
                AdvancedField::HarnessCheckpointRetentionDays,
            ),
            advanced_text(
                theme,
                "Recall limit",
                "Default `memory.recall` top_k (1..=50); blank = built-in default (5).",
                "blank = default",
                self.harness_recall_limit.as_str(),
                AdvancedField::HarnessRecallLimit,
            ),
            advanced_text(
                theme,
                "Verify selector",
                "Default `verify.run` selector; blank = all.",
                "blank = all",
                self.harness_verify_selector.as_str(),
                AdvancedField::HarnessVerifySelector,
            ),
        ]
        .spacing(SPACING_SM);

        let heading = |label: &'a str| text(label).size(13).color(palette.text);

        column![
            text("These dials previously required editing the config file. Blank fields keep the crate default; sections are written on Save Settings.")
                .size(12)
                .color(palette.text_muted),
            heading("Spend"),
            spend,
            heading("Tooling & agent gate"),
            tooling,
            heading("Recovery timeouts"),
            recovery,
            heading("Policy auto-approval window"),
            window,
            heading("Memory retention"),
            memory,
            heading("Skills & project context"),
            extensions,
            heading("Audit"),
            audit,
            heading("Observability"),
            observability,
            heading("Updates"),
            updates,
            heading("Context budget"),
            context,
            heading("Harness"),
            harness,
        ]
        .spacing(SPACING_SM)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::AppConfig;

    fn send(state: &mut State, message: Message) {
        let _ = state.update(message);
    }

    /// Every Advanced field survives a save → reload round-trip, and the
    /// optional sections are published with the edited values.
    #[test]
    fn advanced_fields_round_trip_through_config() {
        let mut state = State::new();
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::SessionSpendCapUsd, "12.5".into()),
        );
        send(&mut state, Message::AdvancedToggled(AdvancedToggle::GitAutoInit, false));
        send(&mut state, Message::AdvancedToggled(AdvancedToggle::StrictVerificationGate, true));
        send(&mut state, Message::AdvancedToggled(AdvancedToggle::PolicyTimeWindowEnabled, true));
        send(&mut state, Message::AdvancedPolicyTimeWindowStartHour(9));
        send(&mut state, Message::AdvancedPolicyTimeWindowEndHour(17));
        send(
            &mut state,
            Message::AdvancedFieldChanged(
                AdvancedField::PolicyTimeWindowTimezone,
                "America/New_York".into(),
            ),
        );
        send(
            &mut state,
            Message::AdvancedFieldChanged(
                AdvancedField::PolicyTimeWindowAutoApproveBelowUsd,
                "0.25".into(),
            ),
        );
        send(
            &mut state,
            Message::AdvancedFieldChanged(
                AdvancedField::MemoryExcludePatterns,
                "target/\nnode_modules/\n".into(),
            ),
        );
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::MemorySummaryKeepPerSession, "7".into()),
        );
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::MemorySummaryRetentionDays, "14".into()),
        );
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::RetryMaxAttempts, "5".into()),
        );
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::ContextTriggerTokens, "20000".into()),
        );
        send(&mut state, Message::AdvancedToggled(AdvancedToggle::ContextCacheStablePrefix, true));
        send(
            &mut state,
            Message::AdvancedFieldChanged(
                AdvancedField::ObservabilityPrometheusPort,
                "9091".into(),
            ),
        );
        send(&mut state, Message::AdvancedToggled(AdvancedToggle::UpdatesCheckOnStartup, false));
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::HarnessRecallLimit, "9".into()),
        );
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::SkillsMaxChars, "1234".into()),
        );
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::ProjectContextMaxBytes, "4096".into()),
        );

        let cfg = state.to_config(&AppConfig::default());

        assert_eq!(cfg.session_spend_cap_usd, Some(12.5));
        assert_eq!(cfg.tool_settings.as_ref().map(|t| t.git_auto_init), Some(false));
        assert_eq!(cfg.agent.as_ref().map(|a| a.strict_verification_gate), Some(true));
        let window = cfg.policy.as_ref().and_then(|p| p.time_window.as_ref()).expect("window");
        assert_eq!(window.start_hour, 9);
        assert_eq!(window.end_hour, 17);
        assert_eq!(window.timezone, "America/New_York");
        assert_eq!(window.auto_approve_below_usd, 0.25);
        assert_eq!(
            cfg.memory.exclude_patterns,
            vec!["target/".to_string(), "node_modules/".to_string()]
        );
        assert_eq!(cfg.memory.summary_keep_per_session, 7);
        assert_eq!(cfg.memory.summary_retention_days, 14);
        assert_eq!(cfg.retry.max_attempts, 5);
        assert_eq!(cfg.context.as_ref().and_then(|c| c.trigger_tokens), Some(20_000));
        assert_eq!(cfg.context.as_ref().and_then(|c| c.cache_stable_prefix), Some(true));
        assert_eq!(cfg.observability.as_ref().and_then(|o| o.prometheus_port), Some(9091));
        assert_eq!(cfg.updates.as_ref().map(|u| u.check_on_startup), Some(false));
        assert_eq!(cfg.harness.as_ref().and_then(|h| h.recall_limit), Some(9));
        assert_eq!(cfg.skills.as_ref().and_then(|s| s.max_chars), Some(1234));
        assert_eq!(cfg.project_context.as_ref().and_then(|p| p.max_bytes), Some(4096));

        // Re-seed from the saved config and confirm the values survive a load.
        let reloaded = State::from_config(&cfg);
        assert_eq!(reloaded.session_spend_cap_usd, "12.5");
        assert!(!reloaded.git_auto_init);
        assert!(reloaded.strict_verification_gate);
        assert!(reloaded.policy_time_window_enabled);
        assert_eq!(reloaded.policy_time_window_timezone, "America/New_York");
        assert_eq!(reloaded.memory_exclude_patterns, "target/\nnode_modules/");
        assert_eq!(reloaded.skills_max_chars, Some(1234));
        assert_eq!(reloaded.project_context_max_bytes, Some(4096));
    }

    /// A blank text field preserves the base/default value instead of clearing
    /// it, and a disabled time window stays absent from the saved config.
    #[test]
    fn blank_fields_preserve_defaults_and_window_stays_absent() {
        let mut state = State::new();
        // Clear the spend cap and leave every other text field blank.
        send(
            &mut state,
            Message::AdvancedFieldChanged(AdvancedField::SessionSpendCapUsd, String::new()),
        );
        let cfg = state.to_config(&AppConfig::default());

        assert_eq!(cfg.session_spend_cap_usd, None);
        assert!(cfg.policy.as_ref().is_some_and(|policy| policy.time_window.is_none()));
        // Untouched optional sections are published at their defaults, never
        // with a spurious non-default value.
        assert_eq!(cfg.retry.max_attempts, AppConfig::default().retry.max_attempts);
        assert_eq!(cfg.context.as_ref().and_then(|c| c.trigger_tokens), None);
        assert_eq!(cfg.harness.as_ref().and_then(|h| h.recall_limit), None);
    }

    /// The Advanced section renders without panicking with the window both on
    /// and off (the window detail fields are conditional).
    #[test]
    fn advanced_section_renders_in_both_window_states() {
        let theme = AppTheme::by_name("Midnight");
        let mut state = State::new();
        let _ = state.advanced_section(&theme);
        state.policy_time_window_enabled = true;
        let _ = state.advanced_section(&theme);
    }
}
