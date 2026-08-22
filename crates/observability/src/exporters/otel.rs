use concerto_config::ObservabilityConfig;
use concerto_core::error::ObservabilityError;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;

/// OTLP exporter that configures and manages an OpenTelemetry tracer provider.
///
/// This exporter sets up an OTLP HTTP exporter with the specified endpoint
/// and wraps it in a `BatchSpanProcessor`.
/// The tracer provider is automatically shut down when this struct is dropped.
pub struct OtelExporter {
    provider: SdkTracerProvider,
}

impl OtelExporter {
    /// Initialize the OTLP exporter based on the provided configuration.
    ///
    /// If the `otlp_endpoint` is `None`, returns a disabled exporter (no-op).
    /// Otherwise, creates a tracer provider configured to export to the specified endpoint.
    pub fn init(config: &ObservabilityConfig) -> Result<Self, ObservabilityError> {
        let endpoint = match config.otlp_endpoint.as_deref() {
            Some(ep) => ep,
            None => return Ok(Self::disabled()),
        };

        // Build the OTLP exporter pipeline using the SDK's batch exporter pattern.
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .with_http_client(reqwest::Client::new())
            .build()
            .map_err(|e| ObservabilityError::OtelInitFailed(e.to_string()))?;

        let resource = Resource::builder()
            .with_attribute(KeyValue::new("service.name", config.service_name.clone()))
            .build();

        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build();

        Ok(Self { provider })
    }

    /// Create a disabled exporter that does nothing.
    /// This is used when OTLP is not configured.
    pub fn disabled() -> Self {
        Self { provider: SdkTracerProvider::builder().build() }
    }

    /// Install this exporter as the global tracer provider.
    ///
    /// This makes the tracer available to `tracing-opentelemetry`.
    pub fn install_global(&self) -> Result<(), ObservabilityError> {
        opentelemetry::global::set_tracer_provider(self.provider.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{Span, Tracer, TracerProvider};

    /// A disabled exporter creates an `SdkTracerProvider` that can produce
    /// a named tracer without panicking.
    #[test]
    fn disabled_exporter_provider_creates_tracers() {
        let exporter = OtelExporter::disabled();
        let tracer = exporter.provider.tracer("test_tracer");
        let _span = tracer.start("test_span");
        // No panic means the provider is functional.
    }

    /// Initializing with `None` OTLP endpoint returns a disabled exporter
    /// whose provider can still create tracers and start spans.
    #[test]
    fn init_none_endpoint_returns_disabled_exporter() {
        let config = ObservabilityConfig::default();
        let exporter = OtelExporter::init(&config).expect("init with None endpoint should succeed");
        let tracer = exporter.provider.tracer("none_test");
        let _span = tracer.start("test_span");
    }

    /// Initializing with a valid endpoint string creates a working provider.
    /// No network call is made at build time — the exporter pipeline is
    /// constructed lazily.
    #[test]
    fn init_valid_endpoint_creates_working_provider() {
        let config = ObservabilityConfig {
            otlp_endpoint: Some("http://localhost:4318".into()),
            service_name: "test-service".into(),
            ..Default::default()
        };
        let exporter = OtelExporter::init(&config)
            .expect("OTLP builder should succeed without a running collector");
        let tracer = exporter.provider.tracer("valid_test");
        let _span = tracer.start("some_operation");
    }

    /// A tracer obtained from the provider can start spans and the span
    /// context is accessible without error.
    #[test]
    fn tracer_created_from_provider_starts_span() {
        let exporter = OtelExporter::disabled();
        let tracer = exporter.provider.tracer("span_tracer");
        let span = tracer.start("my_span");
        let _sc = span.span_context();
    }

    /// Span can be explicitly ended via the `end()` method without panic.
    #[test]
    fn tracer_span_start_and_end_works() {
        let exporter = OtelExporter::disabled();
        let tracer = exporter.provider.tracer("end_tracer");
        let mut span = tracer.start("end_span");
        span.end();
    }

    /// Two `OtelExporter` instances can coexist independently without
    /// interfering with each other's tracer providers.
    #[test]
    fn multiple_otel_exporters_independent() {
        let config1 = ObservabilityConfig {
            otlp_endpoint: Some("http://localhost:4318".into()),
            service_name: "svc-a".into(),
            ..Default::default()
        };
        let config2 = ObservabilityConfig {
            otlp_endpoint: Some("http://localhost:4318".into()),
            service_name: "svc-b".into(),
            ..Default::default()
        };
        let exporter1 = OtelExporter::init(&config1).expect("init1");
        let exporter2 = OtelExporter::init(&config2).expect("init2");
        let tracer1 = exporter1.provider.tracer("t1");
        let tracer2 = exporter2.provider.tracer("t2");
        let span1 = tracer1.start("s1");
        let span2 = tracer2.start("s2");
        drop((span1, span2, tracer1, tracer2));
        // Both exporters exist independently — no shared-state conflict.
    }
}
