use std::sync::Arc;
use wasmtime::{Engine, Store};

use concerto_core::traits::provider::LlmProvider;
use concerto_core::CancellationToken;

use crate::capability::GrantedCapabilities;
use crate::error::PluginError;

/// Shared WASM engine across all plugins (compiled-module cache).
#[derive(Clone)]
pub struct PluginHost {
    engine: Engine,
}

impl PluginHost {
    pub const MAX_FUEL: u64 = 1_000_000;
    pub const MAX_WASM_MODULE_SIZE: usize = 10 * 1024 * 1024; // 10 MB
    pub const DEFAULT_MAX_MEMORY: usize = 64 * 1024 * 1024; // 64 MB
    /// Interval of the background epoch ticker in milliseconds.
    pub const EPOCH_TICKER_INTERVAL_MS: u64 = 100;
    /// Wall-clock budget for WASM execution (belt-and-suspenders with fuel).
    pub const EPOCH_BUDGET_SECS: u64 = 100;
    /// Epoch deadline for WASM interruption (belt-and-suspenders with fuel).
    ///
    /// Each ticker tick increments the engine epoch by one, so the wall-clock
    /// budget is `EPOCH_BUDGET_SECS * 1000 / ticker_interval_ms` ticks. For the
    /// default 100 ms ticker this is 1000 ticks = 100 s. Keep this in sync with
    /// [`Self::EPOCH_TICKER_INTERVAL_MS`] — a deadline computed for the wrong
    /// interval silently turns the interrupt into a no-op for hours.
    pub const EPOCH_DEADLINE: u64 = Self::EPOCH_BUDGET_SECS * 1000 / Self::EPOCH_TICKER_INTERVAL_MS;
    /// Maximum unauthorized host calls before the plugin is disabled.
    pub const MAX_VIOLATIONS: u32 = 1;
}

impl PluginHost {
    pub fn new() -> Result<Self, PluginError> {
        let mut config = wasmtime::Config::default();
        config.wasm_multi_memory(false);
        config.wasm_bulk_memory(true);
        config.wasm_tail_call(false);
        // Load-bearing security tripwire: wasmtime >= 32 defaults memory64 to
        // ON, and RUSTSEC-2026-0096 (aarch64 Cranelift sandbox escape) and
        // RUSTSEC-2026-0086 (Winch + table64 host data leak) require 64-bit
        // linear memories as a precondition. This explicit `false` ensures a
        // future wasmtime version bump cannot silently flip the default and
        // reopen those advisories. The `winch` feature must also stay off
        // (workspace Cargo.toml enables only `cranelift` + `async`).
        config.wasm_memory64(false);
        config.consume_fuel(true);
        config.epoch_interruption(true);
        // Run host functions asynchronously (ADR-38): host functions await host
        // services directly instead of creating a per-call tokio runtime, and
        // in-flight host calls observe the caller's CancellationToken. All
        // stores built from this engine are async stores, so every host-call
        // must go through the wasmtime async API (func_wrap_async /
        // call_async / instantiate_async).
        config.async_support(true);
        // Limit the static linear-memory reservation per instance.
        config.static_memory_maximum_size(Self::DEFAULT_MAX_MEMORY as u64);

        let engine = Engine::new(&config)?;
        Ok(Self { engine })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Create a per-plugin store with default data.
    pub fn create_store(&self) -> Store<PluginStoreData> {
        let mut store = Store::new(&self.engine, PluginStoreData::default());
        if let Err(e) = store.set_fuel(Self::MAX_FUEL) {
            tracing::warn!("failed to set initial fuel: {e}");
        }
        store.set_epoch_deadline(Self::EPOCH_DEADLINE);
        store
    }

    pub fn set_fuel(store: &mut Store<PluginStoreData>, fuel: u64) -> Result<(), PluginError> {
        store.set_fuel(fuel).map_err(PluginError::Runtime)
    }

    /// Start a background epoch ticker that periodically increments the engine
    /// epoch counter, enabling epoch-based WASM interruption.
    ///
    /// Every `interval_ms` the epoch is bumped by one; stores set their
    /// deadline to `EPOCH_DEADLINE`, so with the default 100 ms interval the
    /// budget is `EPOCH_DEADLINE * 100 ms` = `EPOCH_BUDGET_SECS` seconds.
    /// The returned `JoinHandle` can be dropped to stop the ticker.
    pub fn start_epoch_ticker(&self, interval_ms: u64) -> tokio::task::JoinHandle<()> {
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            loop {
                interval.tick().await;
                engine.increment_epoch();
            }
        })
    }
}

/// Per-plugin store data accessible from host function closures.
#[derive(Clone)]
pub struct PluginStoreData {
    pub plugin_id: String,
    pub granted_caps: GrantedCapabilities,
    pub event_bus: Option<tokio::sync::broadcast::Sender<Arc<serde_json::Value>>>,
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub scratch: ScratchBuffer,
    pub last_error: Option<String>,
    pub max_scratch_size: i32,
    pub scratch_resize_count: u32,
    /// Number of unauthorized host function calls made by this plugin.
    pub violation_count: u32,
    /// Whether this plugin has exceeded MAX_VIOLATIONS and is now blocked.
    pub disabled: bool,
    /// Cancellation token observed by in-flight async host calls.
    ///
    /// Set by [`crate::active_plugin::ActivePlugin::set_cancel`] at the tool /
    /// provider / adapter boundaries so host functions such as
    /// `concerto.completion` observe agent/tool-call cancellation. `None`
    /// means no token was threaded through — host functions fall back to a
    /// fresh per-call token (ADR-38 documented fallback).
    pub cancel: Option<CancellationToken>,
}

impl Default for PluginStoreData {
    fn default() -> Self {
        Self {
            plugin_id: String::new(),
            granted_caps: GrantedCapabilities::default(),
            event_bus: None,
            provider: None,
            scratch: ScratchBuffer::default(),
            last_error: None,
            max_scratch_size: 1024 * 1024_i32,
            scratch_resize_count: 0,
            violation_count: 0,
            disabled: false,
            cancel: None,
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct ScratchBuffer {
    pub ptr: i32,
    pub len: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_deadline_gives_wall_clock_budget() {
        // The deadline is a tick count: budget_ms / ticker_interval_ms.
        // For the default 100 ms ticker this must be exactly EPOCH_BUDGET_SECS.
        assert_eq!(
            PluginHost::EPOCH_DEADLINE * PluginHost::EPOCH_TICKER_INTERVAL_MS,
            PluginHost::EPOCH_BUDGET_SECS * 1000,
            "epoch deadline must be derived from the ticker interval"
        );
        // Sanity: the computed deadline is a sane tick count, not the old
        // million-ticks (~27.8 h) value that silently disabled interruption.
        assert_eq!(PluginHost::EPOCH_DEADLINE, 1000);
    }

    #[test]
    fn store_epoch_deadline_matches_ticker() {
        let host = PluginHost::new().expect("host should construct");
        // Smoke test: a store can still be created with the corrected
        // deadline; the constant relationship itself is pinned by
        // `epoch_deadline_gives_wall_clock_budget`.
        let store = host.create_store();
        drop(store);
    }
}
