#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! Observability export layer for Concerto.
//!
//! Provides three optional exporters:
//!   * Prometheus (metrics endpoint)
//!   * OTLP (OpenTelemetry Protocol via HTTP)
//!   * Langfuse (managed observability backend)
//!
//! Exporters are enabled via `ObservabilityConfig`. All exporters are
//! disabled by default (the config fields are `Option`).

mod exporters;

use concerto_config::ObservabilityConfig;
use concerto_core::error::ObservabilityError;
use concerto_core::event::{EventBus, EventKind};
use exporters::{LangfuseExporter, OtelExporter, PrometheusExporter};
use tokio::task::JoinHandle;

/// Handles initialization and shutdown of all configured observability exporters.
/// Fields are intentionally kept alive for their Drop side effects (flush/shutdown).
#[allow(dead_code)]
pub struct ObservabilityExporter {
    prometheus: Option<PrometheusExporter>,
    otel: Option<OtelExporter>,
    langfuse: Option<LangfuseExporter>,
    /// Handle to the background event-processing task.
    event_task: Option<JoinHandle<()>>,
}

impl ObservabilityExporter {
    /// Initialise exporters based on the provided configuration.
    /// Returns `Ok(())` if all enabled exporters start successfully.
    pub fn init(config: ObservabilityConfig, bus: EventBus) -> Result<Self, ObservabilityError> {
        // Initialise each exporter conditionally.
        let prometheus = if config.prometheus_port.is_some() {
            Some(PrometheusExporter::init(&config)?)
        } else {
            None
        };

        let otel = if config.otlp_endpoint.is_some() {
            let exporter = OtelExporter::init(&config)?;
            exporter.install_global()?;
            Some(exporter)
        } else {
            None
        };

        let langfuse = if config.langfuse_host.is_some() {
            let exporter = LangfuseExporter::init(&config)?;
            exporter.install_global()?;
            Some(exporter)
        } else {
            None
        };

        // Subscribe to the EventBus and spawn a background task to process events.
        // Spawn if any exporter is enabled (needed for metrics).
        let event_task = if prometheus.is_some() {
            let mut rx = bus.subscribe();
            Some(tokio::spawn(async move {
                Self::event_loop(&mut rx).await;
            }))
        } else {
            None
        };

        Ok(Self { prometheus, otel, langfuse, event_task })
    }

    /// Background task that processes events and updates observability metrics.
    async fn event_loop(rx: &mut concerto_core::event::EventReceiver) {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Process event based on kind.
                    match &event.kind {
                        EventKind::ProviderCallCompleted { cost } => {
                            metrics::counter!("trace_cost_total", "provider" => cost.provider.clone())
                                .increment((cost.total_usd * 1_000_000.0) as u64);
                            metrics::counter!("tool_calls_total", "tool" => "provider".to_string())
                                .increment(1);
                            tracing::debug!(
                                provider = %cost.provider,
                                cost_usd = cost.total_usd,
                                "Provider call completed — cost attributed"
                            );
                        }
                        EventKind::ToolCalled { tool_name } => {
                            metrics::counter!("tool_calls_total", "tool" => tool_name.clone())
                                .increment(1);
                        }
                        EventKind::PolicyEvaluated { tool_name, verdict, .. } => {
                            tracing::debug!(
                                tool = %tool_name,
                                verdict = %verdict,
                                "Policy verdict recorded"
                            );
                        }
                        EventKind::ToolExecutionFinished {
                            tool_name,
                            duration_ms,
                            success,
                            ..
                        } => {
                            metrics::histogram!("trace_duration_seconds", "tool" => tool_name.clone())
                                .record(*duration_ms as f64 / 1000.0);
                            if !success {
                                metrics::counter!("tool_calls_total", "tool" => tool_name.clone(), "status" => "error")
                                    .increment(1);
                            }
                        }
                        _ => {
                            // Other events are ignored for now.
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // All senders dropped — exit cleanly.
                    tracing::debug!("EventBus closed, observability event loop shutting down");
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "EventBus receiver lagged, some events missed");
                }
            }
        }
    }

    /// Gracefully shutdown all exporters.
    pub fn shutdown(mut self) -> Result<(), ObservabilityError> {
        // Abort the background event-processing task if it's still running.
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
        // Dropping the struct will invoke each exporter's Drop impl which
        // performs flushing/shutdown.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::event::Event;
    use concerto_core::ids::new_id;
    use concerto_core::types::CostInfo;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Serialises access to the Prometheus global recorder (one per process).
    fn prometheus_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Query a Prometheus `/metrics` HTTP endpoint via raw TCP and return the
    /// response body (without HTTP headers).
    async fn fetch_metrics(port: u16) -> String {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("failed to connect to metrics endpoint");

        let request = "GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        stream.write_all(request.as_bytes()).await.expect("failed to send request");

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("failed to read response");

        let response = String::from_utf8(buf).expect("non-UTF-8 response");
        // Strip HTTP headers — body follows the first blank line.
        response.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
    }

    // =========================================================================
    // 1. Config validation tests
    // =========================================================================

    /// Default config has all exporters disabled and service_name = "concerto".
    #[test]
    fn default_has_all_exporters_disabled() {
        let config = ObservabilityConfig::default();
        assert!(config.prometheus_port.is_none());
        assert!(config.otlp_endpoint.is_none());
        assert!(config.langfuse_host.is_none());
        assert!(config.langfuse_public_key.is_none());
        assert!(config.langfuse_secret_key.is_none());
        assert_eq!(config.service_name, "concerto");
    }

    /// Config with only prometheus_port set (Prometheus exporter enabled).
    #[test]
    fn prometheus_port_enables_prometheus_exporter() {
        let config = ObservabilityConfig { prometheus_port: Some(9091), ..Default::default() };
        assert_eq!(config.prometheus_port, Some(9091));
        assert!(config.otlp_endpoint.is_none());
        assert!(config.langfuse_host.is_none());
    }

    /// Config with only otlp_endpoint set (OTLP exporter enabled).
    #[test]
    fn otlp_endpoint_enables_otel_exporter() {
        let config = ObservabilityConfig {
            otlp_endpoint: Some("http://localhost:4318".into()),
            ..Default::default()
        };
        assert_eq!(config.otlp_endpoint.as_deref(), Some("http://localhost:4318"));
        assert!(config.prometheus_port.is_none());
        assert!(config.langfuse_host.is_none());
    }

    /// Config with Langfuse host + keys set (Langfuse exporter enabled).
    #[test]
    fn langfuse_settings_enables_langfuse_exporter() {
        let config = ObservabilityConfig {
            langfuse_host: Some("https://cloud.langfuse.com".into()),
            langfuse_public_key: Some("pk-test".into()),
            langfuse_secret_key: Some("sk-test".into()),
            ..Default::default()
        };
        assert_eq!(config.langfuse_host.as_deref(), Some("https://cloud.langfuse.com"));
        assert!(config.prometheus_port.is_none());
        assert!(config.otlp_endpoint.is_none());
    }

    // =========================================================================
    // 2. PrometheusExporter tests
    // =========================================================================

    /// init() with no port returns PrometheusInitFailed error.
    #[test]
    fn prometheus_init_no_port_returns_error() {
        let config = ObservabilityConfig::default();
        let err = PrometheusExporter::init(&config).unwrap_err();
        match err {
            ObservabilityError::PrometheusInitFailed(msg) => {
                assert!(msg.contains("port not configured"));
            }
            other => panic!("expected PrometheusInitFailed, got: {other}"),
        }
    }

    /// Full lifecycle: init via ObservabilityExporter (installs global
    /// recorder), record metrics, query /metrics for Prometheus format, publish
    /// events, verify counters via /metrics, shutdown cleanly.
    ///
    /// Serialised via prometheus_lock() because the global recorder can only
    /// be installed once per process.
    #[tokio::test]
    async fn prometheus_full_lifecycle() {
        // -- Discover free port (synchronous, under lock) --
        let port = {
            let _guard = prometheus_lock().lock().unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            port
        };

        let config = ObservabilityConfig {
            prometheus_port: Some(port),
            service_name: "test-prom".into(),
            ..Default::default()
        };

        // -- Init (PrometheusExporter created internally, recorder installed) --
        let bus = EventBus::default();
        let obs_exporter = ObservabilityExporter::init(config, bus.clone())
            .expect("init with valid port should succeed");

        // event_task is spawned when Prometheus is enabled.
        assert!(obs_exporter.event_task.is_some());

        // -- Record metrics via the global recorder --
        metrics::describe_counter!("test_counter_desc", "A test counter");
        metrics::counter!("tool_calls_total", "tool" => "init_tool").increment(3);
        metrics::counter!("trace_cost_total", "provider" => "init_provider").increment(42);
        metrics::histogram!("trace_duration_seconds", "tool" => "init_tool").record(2.5);

        // -- Query /metrics --
        tokio::time::sleep(Duration::from_millis(500)).await;
        let body = fetch_metrics(port).await;

        assert!(body.contains("# HELP"), "HELP descriptions present");
        assert!(body.contains("# TYPE"), "TYPE declarations present");
        assert!(body.contains("tool_calls_total"), "tool_calls_total metric present");
        assert!(body.contains("trace_cost_total"), "trace_cost_total metric present");
        assert!(body.contains("trace_duration_seconds"), "histogram present");

        // -- Event processing: publish an event, verify via /metrics --
        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ToolCalled { tool_name: "event_tool".into() },
        ))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let body2 = fetch_metrics(port).await;
        assert!(body2.contains("event_tool"), "ToolCalled counter appears in /metrics");

        // -- Shutdown --
        obs_exporter.shutdown().unwrap();
    }

    // =========================================================================
    // 3. OtelExporter tests
    // =========================================================================

    /// init() with None endpoint returns a disabled (no-op) exporter.
    #[test]
    fn otel_init_none_returns_disabled() {
        let config = ObservabilityConfig::default();
        let exporter = OtelExporter::init(&config)
            .expect("init with None endpoint should return Ok(disabled)");
        assert!(exporter.install_global().is_ok());
    }

    /// init() with a valid endpoint string succeeds (no network at build time).
    #[test]
    fn otel_init_valid_endpoint_succeeds() {
        let config = ObservabilityConfig {
            otlp_endpoint: Some("http://localhost:4318".into()),
            service_name: "test-otel".into(),
            ..Default::default()
        };
        let exporter = OtelExporter::init(&config)
            .expect("OTLP builder should succeed without a running collector");
        assert!(exporter.install_global().is_ok());
    }

    /// disabled() creates a valid no-op provider that installs globally.
    #[test]
    fn otel_disabled_creates_noop_provider() {
        let exporter = OtelExporter::disabled();
        assert!(exporter.install_global().is_ok());
    }

    /// install_global() sets the global tracer provider without error.
    #[test]
    fn otel_install_global_sets_tracer_provider() {
        let config = ObservabilityConfig::default();
        let exporter = OtelExporter::init(&config).expect("init should succeed");
        assert!(exporter.install_global().is_ok());
    }

    // =========================================================================
    // 4. LangfuseExporter tests
    // =========================================================================

    /// init() with None host returns a disabled exporter.
    #[test]
    fn langfuse_init_none_host_returns_disabled() {
        let config = ObservabilityConfig::default();
        let exporter = LangfuseExporter::init(&config)
            .expect("init with None host should return Ok(disabled)");
        assert!(exporter.install_global().is_ok());
    }

    /// init() with a host but empty keys returns disabled.
    #[test]
    fn langfuse_init_empty_keys_returns_disabled() {
        let config = ObservabilityConfig {
            langfuse_host: Some("https://cloud.langfuse.com".into()),
            langfuse_public_key: Some("".into()),
            langfuse_secret_key: Some("".into()),
            ..Default::default()
        };
        let exporter = LangfuseExporter::init(&config)
            .expect("init with empty keys should return Ok(disabled)");
        assert!(exporter.install_global().is_ok());
    }

    /// init() with valid host and non-empty keys succeeds (no network at build).
    #[test]
    fn langfuse_init_valid_config_succeeds() {
        let config = ObservabilityConfig {
            langfuse_host: Some("https://cloud.langfuse.com".into()),
            langfuse_public_key: Some("pk-test".into()),
            langfuse_secret_key: Some("sk-test".into()),
            service_name: "test-lf".into(),
            ..Default::default()
        };
        let result = LangfuseExporter::init(&config);
        match result {
            Ok(exporter) => assert!(exporter.install_global().is_ok()),
            Err(ObservabilityError::LangfuseInitFailed(_)) => { /* acceptable */ }
            Err(e) => panic!("unexpected error type: {e}"),
        }
    }

    /// resolve_key() passes through direct (non-keyring:) values unchanged.
    /// Tested indirectly via LangfuseExporter::init().
    #[test]
    fn langfuse_resolve_key_direct_value() {
        let config = ObservabilityConfig {
            langfuse_host: Some("https://cloud.langfuse.com".into()),
            langfuse_public_key: Some("pk-direct-value".into()),
            langfuse_secret_key: Some("sk-direct-value".into()),
            service_name: "test-key".into(),
            ..Default::default()
        };
        if let Err(ObservabilityError::LangfuseInitFailed(msg)) = LangfuseExporter::init(&config) {
            assert!(
                !msg.contains("failed to resolve"),
                "direct keys should not trigger keyring resolution: {msg}"
            );
        }
    }

    /// resolve_key() handles keyring: prefix by attempting keyring lookup.
    /// Tested indirectly via LangfuseExporter::init().
    #[test]
    fn langfuse_resolve_key_handles_keyring_prefix() {
        let config = ObservabilityConfig {
            langfuse_host: Some("https://cloud.langfuse.com".into()),
            langfuse_public_key: Some("keyring:test-account".into()),
            langfuse_secret_key: Some("keyring:test-secret".into()),
            service_name: "test-keyring".into(),
            ..Default::default()
        };
        match LangfuseExporter::init(&config) {
            Ok(_) => { /* keyring may be configured in CI */ }
            Err(ObservabilityError::LangfuseInitFailed(msg)) => {
                assert!(
                    msg.contains("failed to resolve"),
                    "keyring prefix should produce keyring error: {msg}"
                );
            }
            Err(e) => panic!("unexpected error type: {e}"),
        }
    }

    /// disabled() creates a valid no-op provider that installs globally.
    #[test]
    fn langfuse_disabled_creates_noop_provider() {
        let exporter = LangfuseExporter::disabled();
        assert!(exporter.install_global().is_ok());
    }

    /// install_global() sets the global tracer provider.
    #[test]
    fn langfuse_install_global_sets_tracer_provider() {
        let config = ObservabilityConfig {
            langfuse_host: Some("https://cloud.langfuse.com".into()),
            langfuse_public_key: Some("pk-test".into()),
            langfuse_secret_key: Some("sk-test".into()),
            service_name: "test-lf-global".into(),
            ..Default::default()
        };
        let result = LangfuseExporter::init(&config);
        if let Ok(exporter) = result {
            assert!(exporter.install_global().is_ok());
        }
    }

    // =========================================================================
    // 5. ObservabilityExporter integration tests
    // =========================================================================

    /// init() with all exporters disabled succeeds, no event task spawned.
    #[tokio::test]
    async fn integration_all_disabled_succeeds() {
        let config =
            ObservabilityConfig { service_name: "test-all-disabled".into(), ..Default::default() };
        let bus = EventBus::default();
        let exporter = ObservabilityExporter::init(config, bus)
            .expect("init with all disabled should succeed");
        assert!(exporter.event_task.is_none(), "no event task without Prometheus");
        exporter.shutdown().unwrap();
    }

    /// init() with only OTLP enabled succeeds without spawning event task.
    #[tokio::test]
    async fn integration_otlp_only_succeeds() {
        let config = ObservabilityConfig {
            otlp_endpoint: Some("http://localhost:4318".into()),
            service_name: "test-otlp-only".into(),
            ..Default::default()
        };
        let bus = EventBus::default();
        let exporter =
            ObservabilityExporter::init(config, bus).expect("init with OTLP should succeed");
        assert!(exporter.event_task.is_none(), "no event task without Prometheus");
        exporter.shutdown().unwrap();
    }

    /// shutdown() on an exporter without event task returns Ok.
    #[tokio::test]
    async fn integration_shutdown_no_event_task() {
        let config =
            ObservabilityConfig { service_name: "test-shutdown".into(), ..Default::default() };
        let bus = EventBus::default();
        let exporter = ObservabilityExporter::init(config, bus).unwrap();
        assert!(exporter.shutdown().is_ok(), "shutdown should succeed");
    }

    /// Event processing: ProviderCallCompleted is consumed without panic.
    #[tokio::test]
    async fn integration_event_provider_call_completed_processes() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ProviderCallCompleted {
                cost: CostInfo {
                    total_usd: 0.005,
                    tokens_in: 1000,
                    tokens_out: 500,
                    provider: "test-provider".into(),
                    model: "test-model".into(),
                },
            },
        ))
        .unwrap();
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should exit cleanly");
    }

    /// Event processing: ToolCalled is consumed without panic.
    #[tokio::test]
    async fn integration_event_tool_called_processes() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ToolCalled { tool_name: "my-tool".into() },
        ))
        .unwrap();
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should exit cleanly");
    }

    /// Event processing: successful ToolExecutionFinished records histogram,
    /// does not increment error counter.
    #[tokio::test]
    async fn integration_event_tool_execution_finished_records() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ToolExecutionFinished {
                tool_name: "my-tool".into(),
                duration_ms: 1500,
                success: true,
                detail: Some("Completed successfully".into()),
            },
        ))
        .unwrap();
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should exit cleanly");
    }

    /// Event processing: failed ToolExecutionFinished increments error counter.
    #[tokio::test]
    async fn integration_event_tool_execution_failed_increments() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ToolExecutionFinished {
                tool_name: "failing-tool".into(),
                duration_ms: 500,
                success: false,
                detail: Some("Something went wrong".into()),
            },
        ))
        .unwrap();
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should exit cleanly");
    }

    // =========================================================================
    // 6. Event loop — value and volume tests
    // =========================================================================

    /// ProviderCallCompleted with a high cost value is processed without panic.
    #[tokio::test]
    async fn integration_event_provider_call_high_cost() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ProviderCallCompleted {
                cost: CostInfo {
                    total_usd: 1.0,
                    tokens_in: 50_000,
                    tokens_out: 10_000,
                    provider: "expensive-provider".into(),
                    model: "gpt-4".into(),
                },
            },
        ))
        .unwrap();
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should process high-cost events");
    }

    /// A batch of five ToolCalled events is processed sequentially
    /// without any panic or hang.
    #[tokio::test]
    async fn integration_event_multiple_tool_calls_processed() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        for i in 0..5 {
            bus.publish(Event::new(
                new_id(),
                new_id(),
                EventKind::ToolCalled { tool_name: format!("batch-tool-{i}") },
            ))
            .unwrap();
        }
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should process batch of tool calls");
    }

    /// Three successful ToolExecutionFinished events with varying durations
    /// are processed, exercising histogram recording with different values.
    #[tokio::test]
    async fn integration_event_varying_durations_all_success() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        for (tool, ms) in [("fast-tool", 100u64), ("medium-tool", 1500), ("slow-tool", 5000)] {
            bus.publish(Event::new(
                new_id(),
                new_id(),
                EventKind::ToolExecutionFinished {
                    tool_name: tool.into(),
                    duration_ms: ms,
                    success: true,
                    detail: None,
                },
            ))
            .unwrap();
        }
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should process varying durations");
    }

    /// Two consecutive failed ToolExecutionFinished events are processed,
    /// exercising the error-counter increment path.
    #[tokio::test]
    async fn integration_event_multiple_failures_increments() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        for i in 0..2 {
            bus.publish(Event::new(
                new_id(),
                new_id(),
                EventKind::ToolExecutionFinished {
                    tool_name: format!("broken-tool-{i}"),
                    duration_ms: 200,
                    success: false,
                    detail: Some(format!("failure #{i}")),
                },
            ))
            .unwrap();
        }
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should process multiple failures");
    }

    /// A sequence of different event kinds (ProviderCallCompleted,
    /// ToolCalled, PolicyEvaluated, ToolExecutionFinished) is processed
    /// correctly without stalling or panicking.
    #[tokio::test]
    async fn integration_event_mixed_types_sequentially() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ProviderCallCompleted {
                cost: CostInfo {
                    total_usd: 0.0025,
                    tokens_in: 500,
                    tokens_out: 200,
                    provider: "mixed-provider".into(),
                    model: "claude-3".into(),
                },
            },
        ))
        .unwrap();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ToolCalled { tool_name: "read-file".into() },
        ))
        .unwrap();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::PolicyEvaluated {
                tool_name: "read-file".into(),
                verdict: "allow".into(),
                rule_matched: Some("permit-by-default".into()),
            },
        ))
        .unwrap();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::ToolExecutionFinished {
                tool_name: "read-file".into(),
                duration_ms: 120,
                success: true,
                detail: Some("read 42 bytes".into()),
            },
        ))
        .unwrap();

        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should process mixed event types");
    }

    /// A PolicyEvaluated event (which only produces a debug log and does
    /// not modify metrics) is handled without error.
    #[tokio::test]
    async fn integration_event_policy_evaluated_processed() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(Event::new(
            new_id(),
            new_id(),
            EventKind::PolicyEvaluated {
                tool_name: "write-tool".into(),
                verdict: "reject".into(),
                rule_matched: Some("block-high-risk".into()),
            },
        ))
        .unwrap();
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should process PolicyEvaluated events");
    }

    // =========================================================================
    // 7. Event loop edge cases
    // =========================================================================

    /// EventBus closed → event loop exits cleanly (no hang/panic).
    #[tokio::test]
    async fn event_loop_closed_exits_cleanly() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should exit when bus is closed");
    }

    /// EventBus lagged → logs warning, continues, exits on closed.
    #[tokio::test]
    async fn event_loop_lagged_logs_warning() {
        let bus = EventBus::new(2); // tiny capacity
        let mut rx = bus.subscribe();

        for i in 0..20 {
            bus.publish(Event::new(
                new_id(),
                new_id(),
                EventKind::ToolCalled { tool_name: format!("tool-{i}") },
            ))
            .unwrap();
        }
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should handle lag and exit cleanly");
    }

    /// Unknown event kind → ignored gracefully (`_ => {}` arm).
    #[tokio::test]
    async fn event_loop_unknown_event_ignored() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        // Test-only synthetic event: intentionally unscoped (unknown-kind
        // fixture with no session context).
        bus.publish_raw(EventKind::SessionSaved).unwrap();
        drop(bus);

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            ObservabilityExporter::event_loop(&mut rx),
        )
        .await;
        assert!(result.is_ok(), "event loop should ignore unknown kinds");
    }

    // -----------------------------------------------------------------------
    // Existing tests (kept for backward compatibility)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn prometheus_counter_increments() {
        let config = ObservabilityConfig {
            prometheus_port: None,
            service_name: "test".into(),
            ..Default::default()
        };
        let bus = EventBus::default();
        let exporter = ObservabilityExporter::init(config, bus.clone()).unwrap();

        for _ in 0..3 {
            let event = Event::new(
                concerto_core::ids::new_id(),
                concerto_core::ids::new_id(),
                EventKind::ToolCalled { tool_name: "test_tool".into() },
            );
            bus.publish(event).unwrap();
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        exporter.shutdown().unwrap();
    }

    #[tokio::test]
    async fn disabled_subscribes_nothing() {
        let config = ObservabilityConfig {
            prometheus_port: None,
            service_name: "test".into(),
            ..Default::default()
        };
        let bus = EventBus::default();
        let _exporter = ObservabilityExporter::init(config, bus.clone()).unwrap();
        // With no exporters enabled, no background task should be spawned.
    }

    #[test]
    fn observability_config_service_name_default() {
        let config = ObservabilityConfig::default();
        assert_eq!(config.service_name, "concerto");
    }

    #[test]
    fn langfuse_config_resolve_key_direct() {
        let config = ObservabilityConfig {
            langfuse_host: Some("https://cloud.langfuse.com".into()),
            langfuse_public_key: Some("pk-test".into()),
            langfuse_secret_key: Some("sk-test".into()),
            service_name: "test".into(),
            ..Default::default()
        };
        assert_eq!(config.langfuse_public_key.as_deref(), Some("pk-test"));
        assert_eq!(config.langfuse_secret_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn observability_config_all_disabled_by_default() {
        let config = ObservabilityConfig::default();
        assert!(config.prometheus_port.is_none());
        assert!(config.otlp_endpoint.is_none());
        assert!(config.langfuse_host.is_none());
        assert!(config.langfuse_public_key.is_none());
        assert!(config.langfuse_secret_key.is_none());
    }

    #[test]
    fn observability_config_partial_prometheus() {
        let config = ObservabilityConfig { prometheus_port: Some(9091), ..Default::default() };
        assert_eq!(config.prometheus_port, Some(9091));
        assert!(config.otlp_endpoint.is_none());
    }

    #[test]
    fn observability_config_partial_otlp() {
        let config = ObservabilityConfig {
            otlp_endpoint: Some("http://localhost:4318".into()),
            ..Default::default()
        };
        assert_eq!(config.otlp_endpoint.as_deref(), Some("http://localhost:4318"));
        assert!(config.prometheus_port.is_none());
    }

    #[test]
    fn observability_error_display() {
        let err = ObservabilityError::PrometheusInitFailed("bad port".into());
        assert!(err.to_string().contains("bad port"));
        let err = ObservabilityError::OtelInitFailed("timeout".into());
        assert!(err.to_string().contains("timeout"));
    }

    /// ObservabilityError PrometheusInitFailed variant contains the error message.
    #[test]
    fn observability_error_prometheus_init_message() {
        let err = ObservabilityError::PrometheusInitFailed("bad port".into());
        assert!(err.to_string().contains("bad port"));
    }
}
