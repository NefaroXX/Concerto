//! `concerto-api-server` binary entry point.
//!
//! Starts an axum HTTP server on a configurable port (default 3000).
//!
//! Usage:
//!   concerto-api-server                    # Start server on port 3000
//!   concerto-api-server --generate-openapi # Print OpenAPI spec to stdout and exit

use concerto_api_server::routes;
use concerto_api_server::state::AppState;
use concerto_core::event::EventBus;
use concerto_sessions::SqliteSessionStore;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use utoipa::OpenApi;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Check for --generate-openapi flag before any other setup.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--generate-openapi") {
        let spec = routes::ApiDoc::openapi()
            .to_json()
            .map_err(|e| anyhow::anyhow!("failed to serialize OpenAPI spec: {e}"))?;
        println!("{spec}");
        return Ok(());
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let port: u16 =
        std::env::var("CONCERTO_API_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(3000);

    let store = SqliteSessionStore::connect()
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to session store: {e}"))?;

    // ADR-44 §1/§2: the project-root allowlist. Read `CONCERTO_PROJECT_ROOTS`
    // directly (path-separated) rather than via concerto_config, so this value
    // can never diverge from the non-loopback startup gate below, which reads
    // the same env var. Populated into AppState so `create_session` enforces
    // out-of-root confinement. Non-UTF-8 entries are skipped with a warning.
    let project_roots: Vec<camino::Utf8PathBuf> = std::env::var("CONCERTO_PROJECT_ROOTS")
        .map(|raw| {
            std::env::split_paths(&raw)
                .filter_map(|p| {
                    camino::Utf8PathBuf::from_path_buf(p)
                        .map_err(|p| {
                            tracing::warn!(
                                "ignoring non-UTF-8 path in CONCERTO_PROJECT_ROOTS: {p:?}"
                            );
                        })
                        .ok()
                })
                .collect()
        })
        .unwrap_or_default();

    let state = AppState { bus: EventBus::default(), store: Arc::new(store), project_roots };

    let app = routes::router(state);

    let host = std::env::var("CONCERTO_API_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let ip: std::net::IpAddr =
        host.parse().map_err(|e| anyhow::anyhow!("Invalid CONCERTO_API_HOST: {e}"))?;
    let addr = SocketAddr::from((ip, port));

    // ADR-44 §3: exposing the server on a non-loopback address is restricted
    // by default. It requires both an API key and a non-empty project-root
    // allowlist so that remote callers cannot root sessions at arbitrary
    // filesystem locations. Loopback binding stays permissive (unchanged
    // local behavior).
    if !ip.is_loopback() {
        let api_key_set =
            std::env::var("CONCERTO_API_KEY").ok().filter(|s| !s.is_empty()).is_some();
        let project_roots_set = std::env::var("CONCERTO_PROJECT_ROOTS")
            .ok()
            .map(|v| std::env::split_paths(&v).any(|p| !p.as_os_str().is_empty()))
            .unwrap_or(false);
        if !api_key_set || !project_roots_set {
            anyhow::bail!(
                "Binding to a non-localhost address requires CONCERTO_API_KEY and \
                 a non-empty CONCERTO_PROJECT_ROOTS ({separator}-separated list \
                 of allowed project root paths) to be set",
                separator = std::path::MAIN_SEPARATOR,
            );
        }
        tracing::warn!(
            "concerto-api-server bound to non-loopback address {} — this exposes \
             the automation service to the network. Use network-layer access \
             controls, a strong API key, and a restrictive CONCERTO_PROJECT_ROOTS \
             allowlist.",
            addr,
        );
    }
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("concerto-api-server listening on http://{oml}", oml = addr);

    axum::serve(listener, app).await?;

    Ok(())
}
