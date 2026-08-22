//! Exporter module aggregating concrete observability backends.
//!
//! Three optional exporters: Prometheus, OTLP, Langfuse.
//!
//! Each exporter is enabled only when the corresponding field in
//! `ObservabilityConfig` is `Some`. The top‑level `ObservabilityExporter`
//! orchestrates their lifecycle.

pub mod langfuse;
pub mod otel;
pub mod prometheus;

pub use langfuse::LangfuseExporter;
pub use otel::OtelExporter;
pub use prometheus::PrometheusExporter;
