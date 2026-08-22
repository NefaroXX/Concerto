use concerto_config::credentials::CredentialStore;
use concerto_config::ObservabilityConfig;
use concerto_core::error::ObservabilityError;
use opentelemetry::KeyValue;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;

/// Langfuse exporter that configures and manages an OpenTelemetry tracer provider.
///
/// This exporter sets up a Langfuse SpanExporter with the specified host
/// and Basic auth credentials, and wraps it in a `BatchSpanProcessor`.
/// The tracer provider is automatically shut down when this struct is dropped.
pub struct LangfuseExporter {
    provider: SdkTracerProvider,
}

impl LangfuseExporter {
    /// Initialize the Langfuse exporter based on the provided configuration.
    ///
    /// If `langfuse_host` is `None`, or if either key is missing, returns a
    /// disabled exporter (no-op).
    pub fn init(config: &ObservabilityConfig) -> Result<Self, ObservabilityError> {
        let host = match config.langfuse_host.as_deref() {
            Some(h) => h,
            None => return Ok(Self::disabled()),
        };

        let public_key = config.langfuse_public_key.as_deref().unwrap_or("");
        let secret_key = config.langfuse_secret_key.as_deref().unwrap_or("");

        if public_key.is_empty() || secret_key.is_empty() {
            return Ok(Self::disabled());
        }

        let public_key = Self::resolve_key(public_key, "langfuse_public_key")?;
        let secret_key = Self::resolve_key(secret_key, "langfuse_secret_key")?;

        // Build the Langfuse exporter.
        let exporter = opentelemetry_langfuse::ExporterBuilder::new()
            .with_host(host)
            .with_basic_auth(&public_key, &secret_key)
            .build()
            .map_err(|e| ObservabilityError::LangfuseInitFailed(e.to_string()))?;

        let resource = Resource::builder()
            .with_attribute(KeyValue::new("service.name", config.service_name.clone()))
            .build();

        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build();

        Ok(Self { provider })
    }

    /// Resolve a key from either a direct value or a keyring reference.
    ///
    /// If the value starts with `"keyring:"`, resolve it from the
    /// `CredentialStore`.  Otherwise return the value as-is.
    fn resolve_key(value: &str, key_name: &str) -> Result<String, ObservabilityError> {
        if let Some(account) = value.strip_prefix("keyring:") {
            let store = CredentialStore::new();
            store.get(account).map_err(|e| {
                ObservabilityError::LangfuseInitFailed(format!(
                    "failed to resolve {key_name} from keyring: {e}"
                ))
            })
        } else {
            Ok(value.to_owned())
        }
    }

    /// Create a disabled exporter that does nothing.
    /// This is used when Langfuse is not configured.
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
