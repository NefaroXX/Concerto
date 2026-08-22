//! Builder for [`SharedServices`].
//!
//! Reduces the ~20‑line inline construction in each frontend to a single
//! chain.  Accepts a pre-existing shared-mutable wrapper so that memory
//! state persists across calls.

use std::sync::{Arc, Mutex};

use concerto_config::AppConfig;
use concerto_core::event::EventBus;
use concerto_core::traits::approval::ApprovalSink;
use concerto_tools::virtual_fs::VirtualFs;

use crate::runtime_runner::{ActiveMemoryServices, SharedServices};
use crate::session_manager::ProjectSessionManager;

/// Builder for [`SharedServices`].
///
/// # Required
/// - `bus`, `config`, `approval_sink` — passed to [`new`](ServicesBuilder::new).
///
/// # Optional
/// - [`with_vfs`](ServicesBuilder::with_vfs) — enables the virtual filesystem (Desktop).
/// - [`with_session_manager`](ServicesBuilder::with_session_manager) — persistent session store.
/// - [`with_memory`](ServicesBuilder::with_memory) — pre-existing memory wrapper (reused across calls).
///
/// # Build
/// - [`build`](ServicesBuilder::build) — produces [`SharedServices`].
pub struct ServicesBuilder {
    bus: EventBus,
    config: AppConfig,
    approval_sink: Arc<dyn ApprovalSink>,
    vfs: Option<Arc<Mutex<VirtualFs>>>,
    session_manager: Option<Arc<ProjectSessionManager>>,
    memory: Arc<Mutex<Option<ActiveMemoryServices>>>,
}

impl ServicesBuilder {
    /// Start building shared services with a fresh shared-mutable wrapper.
    pub fn new(bus: EventBus, config: AppConfig, approval_sink: Arc<dyn ApprovalSink>) -> Self {
        Self {
            bus,
            config,
            approval_sink,
            vfs: None,
            session_manager: None,
            memory: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach a virtual filesystem (Desktop-only).
    pub fn with_vfs(mut self, vfs: Arc<Mutex<VirtualFs>>) -> Self {
        self.vfs = Some(vfs);
        self
    }

    /// Attach a session manager for persistent conversations.
    pub fn with_session_manager(mut self, manager: Arc<ProjectSessionManager>) -> Self {
        self.session_manager = Some(manager);
        self
    }

    /// Reuse a pre-existing shared-memory wrapper so that cached memory
    /// services persist across calls.
    ///
    /// Pass the same `Arc<Mutex<Option<ActiveMemoryServices>>>` on every call
    /// from the same frontend App.
    pub fn with_memory(mut self, memory: Arc<Mutex<Option<ActiveMemoryServices>>>) -> Self {
        self.memory = memory;
        self
    }

    /// Consume the builder and produce a [`SharedServices`] bundle.
    ///
    /// The skills context is built from `config.skills` and refreshed once so
    /// the first prompt build is served from discovery results. Discovery
    /// failures are logged and swallowed here (fail-soft, ADR-43): a broken
    /// skill pack must never prevent the agent loop from starting. The MCP
    /// manager is constructed from `config.mcp` but spawns **no** processes —
    /// servers start on the first `register_tools` call.
    pub fn build(self) -> SharedServices {
        let skills = Arc::new(crate::skills_context::SkillsContext::from_config(
            self.config.skills.as_ref(),
        ));
        if let Err(error) = skills.refresh() {
            tracing::warn!(%error, "skills discovery failed at startup; continuing without skills");
        }
        let mcp = Arc::new(concerto_mcp::McpManager::new(
            self.config.mcp.clone().unwrap_or_default(),
            self.bus.clone(),
        ));
        SharedServices {
            bus: self.bus,
            config: self.config,
            memory: self.memory,
            vfs: self.vfs,
            approval_sink: self.approval_sink,
            session_manager: self.session_manager,
            skills,
            mcp,
        }
    }
}
