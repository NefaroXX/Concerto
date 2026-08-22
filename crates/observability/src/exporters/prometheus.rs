use axum::{routing::get, Router};
use concerto_config::ObservabilityConfig;
use concerto_core::error::ObservabilityError;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::net::TcpListener as StdTcpListener;

/// Prometheus exporter that installs a metrics recorder and serves a `/metrics`
/// HTTP endpoint on the configured port.
#[derive(Debug)]
pub struct PrometheusExporter {
    // Kept alive to prevent the metrics recorder from being dropped.
    _handle: PrometheusHandle,
}

impl PrometheusExporter {
    /// Initialise the Prometheus exporter if a port is configured.
    ///
    /// Binds the HTTP listener synchronously for immediate error feedback
    /// (port conflicts surface as `Err`), then spawns the Axum server in
    /// a background tokio task.
    pub fn init(config: &ObservabilityConfig) -> Result<Self, ObservabilityError> {
        let port = config.prometheus_port.ok_or_else(|| {
            ObservabilityError::PrometheusInitFailed("port not configured".into())
        })?;

        let handle = PrometheusBuilder::new()
            .install_recorder()
            .map_err(|e| ObservabilityError::PrometheusInitFailed(e.to_string()))?;

        // Register metric descriptions.
        metrics::describe_histogram!(
            "trace_duration_seconds",
            "Duration of trace spans in seconds"
        );
        metrics::describe_counter!("trace_cost_total", "Total cost attributed to traces");
        metrics::describe_counter!("tool_calls_total", "Total number of tool calls");

        // Bind the listener synchronously so port conflicts surface at init time.
        // Default to localhost so the /metrics endpoint is not unexpectedly
        // exposed on public interfaces.
        let std_listener = StdTcpListener::bind(format!("127.0.0.1:{port}")).map_err(|e| {
            ObservabilityError::PrometheusInitFailed(format!("failed to bind port {port}: {e}"))
        })?;
        // Set to non-blocking mode before converting to tokio listener.
        std_listener.set_nonblocking(true).map_err(|e| {
            ObservabilityError::PrometheusInitFailed(format!(
                "failed to set non-blocking mode: {e}"
            ))
        })?;
        let bound_port = std_listener
            .local_addr()
            .map_err(|e| {
                ObservabilityError::PrometheusInitFailed(format!("failed to get bound port: {e}"))
            })?
            .port();

        let app = Router::new().route(
            "/metrics",
            get({
                let handle = handle.clone();
                move || {
                    let handle = handle.clone();
                    async move {
                        (
                            [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                            handle.render(),
                        )
                    }
                }
            }),
        );

        let tokio_listener = tokio::net::TcpListener::from_std(std_listener).map_err(|e| {
            ObservabilityError::PrometheusInitFailed(format!(
                "failed to convert to tokio listener: {e}"
            ))
        })?;

        tracing::info!(port = bound_port, "Prometheus metrics endpoint bound");
        tokio::spawn(async move {
            if let Err(e) = axum::serve(tokio_listener, app).await {
                tracing::error!(error = %e, "Prometheus HTTP server failed");
            }
        });

        Ok(Self { _handle: handle })
    }
}

#[cfg(test)]
mod tests {
    use metrics::{Key, KeyName, Label, Level, Metadata, Recorder};
    use metrics_exporter_prometheus::PrometheusBuilder;

    /// Building a Prometheus recorder without installing it globally succeeds.
    /// The returned recorder and handle can be used locally for testing metrics
    /// rendering without affecting the process-wide global recorder.
    #[test]
    fn build_recorder_without_install_succeeds() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let _handle = recorder.handle();
    }

    /// Counter metrics incremented through a local recorder appear in the
    /// rendered Prometheus exposition format output.
    #[test]
    fn counter_renders_in_prometheus_format() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let key = Key::from_name("test_counter");
        let metadata = Metadata::new("test", Level::INFO, None);
        let counter = recorder.register_counter(&key, &metadata);
        counter.increment(42);

        let output = handle.render();
        assert!(output.contains("test_counter"), "counter name should appear in render output");
        assert!(output.contains("42"), "counter value 42 should appear in render output");
    }

    /// Histogram metrics produce all expected Prometheus histogram elements:
    /// `_bucket` (with le labels), `_count`, and `_sum`.
    #[test]
    fn histogram_renders_with_distribution() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let key = Key::from_name("test_histogram");
        let metadata = Metadata::new("test", Level::INFO, None);
        let histogram = recorder.register_histogram(&key, &metadata);
        histogram.record(1.5);
        histogram.record(2.5);

        let output = handle.render();
        assert!(output.contains("test_histogram"), "histogram name should appear");
        assert!(output.contains("quantile="), "histogram quantiles should be present");
        assert!(output.contains("_count"), "histogram count should be present");
        assert!(output.contains("_sum"), "histogram sum should be present");
    }

    /// A metric registered with a description via `describe_counter` produces
    /// a `# HELP` line in the Prometheus output.
    #[test]
    fn describe_creates_help_line() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let key = Key::from_name("described_metric");
        let metadata = Metadata::new("test", Level::INFO, None);
        recorder.describe_counter(
            KeyName::from("described_metric"),
            None,
            metrics::SharedString::const_str("A test description"),
        );
        let _ = recorder.register_counter(&key, &metadata);

        let output = handle.render();
        assert!(output.contains("# HELP"), "HELP line should be present");
        assert!(output.contains("described_metric"), "metric name should appear in HELP");
        assert!(output.contains("A test description"), "description text should appear in HELP");
    }

    /// Each registered metric has a `# TYPE` declaration in the Prometheus output.
    #[test]
    fn type_declaration_appears_for_metric() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let key = Key::from_name("typed_metric");
        let metadata = Metadata::new("test", Level::INFO, None);
        let _ = recorder.register_counter(&key, &metadata);

        let output = handle.render();
        assert!(output.contains("# TYPE"), "TYPE line should be present");
        assert!(output.contains("typed_metric"), "metric name should appear in TYPE");
    }

    /// Counters with different label sets are rendered as separate time series,
    /// each carrying their respective label key-value pairs.
    #[test]
    fn labeled_counters_show_labels_in_output() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let key1 = Key::from_parts("labeled_counters", vec![Label::new("env", "prod")]);
        let key2 = Key::from_parts("labeled_counters", vec![Label::new("env", "staging")]);
        let meta = Metadata::new("test", Level::INFO, None);
        let c1 = recorder.register_counter(&key1, &meta);
        let c2 = recorder.register_counter(&key2, &meta);
        c1.increment(10);
        c2.increment(20);

        let output = handle.render();
        assert!(
            output.contains("env={\"prod\"") || output.contains("env=\"prod\""),
            "prod label should appear in output, got: {output}"
        );
        assert!(
            output.contains("env={\"staging\"}") || output.contains("env=\"staging\""),
            "staging label should appear in output"
        );
    }

    /// A histogram registered with a description via `describe_histogram` produces
    /// a `# HELP` line containing the description text in the rendered output.
    #[test]
    fn histogram_description_appears_in_help() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let key = Key::from_name("histo_with_desc");
        let metadata = Metadata::new("test", Level::INFO, None);
        recorder.describe_histogram(
            KeyName::from("histo_with_desc"),
            None,
            metrics::SharedString::const_str("Histogram description"),
        );
        let _ = recorder.register_histogram(&key, &metadata);

        let output = handle.render();
        assert!(output.contains("# HELP"), "HELP line should be present");
        assert!(output.contains("histo_with_desc"), "metric name should appear");
        assert!(output.contains("Histogram description"), "description should appear in HELP");
    }

    /// An incremented counter renders its value correctly in the output.
    #[test]
    fn counter_value_matches_incremented_amount() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let key = Key::from_name("value_counter");
        let metadata = Metadata::new("test", Level::INFO, None);
        let counter = recorder.register_counter(&key, &metadata);
        counter.increment(1);

        let output = handle.render();
        assert!(
            output.contains("value_counter 1"),
            "counter render should contain 'value_counter 1'"
        );
    }
}
