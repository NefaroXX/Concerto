//! Runtime-owned manager for MCP servers (ADR-43, decision 7; plan §6 C3).
//!
//! # Lifecycle
//!
//! [`McpManager`] is constructed once per runtime from the [`McpConfig`] and
//! the shared [`EventBus`]; construction spawns **no** processes. Servers are
//! spawned only when [`McpManager::register_tools`] is called (startup, after
//! plugin tools are registered) or when [`McpManager::start_server`] is
//! invoked from the UI (Task 7). Each enabled server owns exactly one live
//! [`McpClient`] (`McpClient::spawn` is a double-spawn guard). Every remote
//! tool is bridged into the shared [`ToolRegistry`] as an [`McpTool`] with the
//! namespaced name `mcp:<server_id>:<tool_name>`.
//!
//! # Collision policy
//!
//! MCP registration is collision-checked (ADR-43 §4): `ToolRegistry::get` is
//! consulted for every namespaced name and a hit is a **hard error**
//! ([`McpError::DuplicateTool`]) — nothing is ever silently clobbered.
//! Duplicate server ids are rejected at config validation; the manager
//! re-checks as defense in depth. At startup a duplicate tool name rolls back
//! that server's partially-registered tools, marks the server `Failed`, and
//! logs loudly — one bad server never blocks the rest, and the UI surfaces the
//! `Failed` state (ADR-43 §7). An interactive [`Self::start_server`] call
//! propagates the error to its caller instead. A server whose
//! `initialize`/`tools/list` fails is likewise marked `Failed` and skipped.
//!
//! # Default posture
//!
//! MCP tools are ordinary tools under `ToolExecutor`, so policy, spend, audit
//! and events apply unchanged (ADR-43 §6). The orchestrator appends an
//! `mcp:*` → `RequireApproval` preset rule **after** user rules so unmatched
//! MCP tools are never implicitly auto-approved; explicit user rules placed
//! earlier win by first-match-wins.
//!
//! # Events
//!
//! A per-server watcher task subscribes to the client's state watch channel
//! and publishes [`EventKind::McpServerStateChanged`] for every transition:
//! `Connecting` after spawn, `Connected` after `initialize` + `tools/list`,
//! `Failed` (with the error detail) on crash/exit or registration failure,
//! `Stopped` on stop. A `Failed` transition also **clears the server handle's
//! tools** (tools marked unavailable): the UI lists zero tools and later runs
//! do not re-register them until the server is reconnected via
//! [`McpManager::start_server`]. Tools already handed to the current run's
//! registry are not revoked — a call against a dead server fails cleanly
//! through the bridge. Disabled servers are never started and emit nothing.
//!
//! # Teardown
//!
//! [`McpManager::stop_all`] gracefully stops every live server and removes
//! its tools from the registry. If the manager is simply dropped (end of an
//! agent run), each [`McpClient`] `Drop` impl SIGKILLs and reaps its child so
//! no server is orphaned.

use crate::client::McpClient;
use crate::error::McpError;
use crate::tool_bridge::{McpTool, DEFAULT_TIMEOUT_SECS};
use concerto_api_types::extension::McpToolDescriptor;
use concerto_config::{McpConfig, McpServerConfig};
use concerto_core::event::{EventBus, EventKind};
use concerto_core::types::ToolRegistry;
use concerto_core::{CancellationToken, McpServerState};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock, Weak};
use tokio::sync::{watch, Mutex};

/// One MCP server under manager control: its live client, the bridged tools
/// currently registered in the shared `ToolRegistry`, and a receiver for the
/// client's lifecycle state.
pub struct McpServerHandle {
    client: Arc<Mutex<McpClient>>,
    tools: RwLock<Vec<McpTool>>,
    state_rx: watch::Receiver<McpServerState>,
}

impl McpServerHandle {
    /// The shared client handle; callers that need a server-wide operation
    /// (e.g. `ping`) lock it directly.
    pub fn client(&self) -> &Arc<Mutex<McpClient>> {
        &self.client
    }

    /// Current lifecycle state ([`McpServerState`]).
    pub fn state(&self) -> McpServerState {
        *self.state_rx.borrow()
    }

    /// Number of tools currently registered for this server.
    pub fn tool_count(&self) -> usize {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The namespaced names (`mcp:<server_id>:<tool>`) of the registered tools.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|tool| tool.tool_name().to_string())
            .collect()
    }

    /// The unqualified descriptors of the registered tools, for UI listing.
    pub fn tool_descriptors(&self) -> Vec<McpToolDescriptor> {
        self.tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|tool| tool.descriptor().clone())
            .collect()
    }

    fn replace_tools(&self, tools: Vec<McpTool>) {
        *self.tools.write().unwrap_or_else(|e| e.into_inner()) = tools;
    }

    fn clear_tools(&self) {
        self.tools.write().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Runtime-owned manager for MCP stdio servers (ADR-43).
pub struct McpManager {
    config: McpConfig,
    /// Live handles keyed by server id. Handles stay in the map after a
    /// graceful stop (state `Stopped`) so the UI can render them.
    servers: RwLock<HashMap<String, Arc<McpServerHandle>>>,
    bus: EventBus,
}

impl McpManager {
    /// Create a manager from config. Spawns nothing — processes start on
    /// [`Self::register_tools`] / [`Self::start_server`].
    pub fn new(config: McpConfig, bus: EventBus) -> Self {
        Self { config, servers: RwLock::new(HashMap::new()), bus }
    }

    /// Whether the MCP client is globally enabled in config.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Start every enabled server and register its tools into `registry`.
    ///
    /// Call this at runtime startup **after** plugin tools are registered so
    /// MCP tools can never clobber them, and again on every subsequent agent
    /// run — the runtime builds a fresh `ToolRegistry` per run, so tools of an
    /// already-connected server are re-bridged from the manager's handle
    /// (clones; no re-spawn). A server whose spawn/`initialize`/`tools/list`
    /// fails is marked `Failed` and skipped (one bad server does not block the
    /// rest); a duplicate tool name rolls back the offending server's
    /// partially-registered tools, marks it `Failed`, and logs loudly before
    /// continuing — startup always completes, and the `Failed` state is
    /// surfaced through [`Self::server_state`] / [`Self::servers`] and the
    /// `McpServerStateChanged` event.
    pub async fn register_tools(&self, registry: &mut ToolRegistry) -> Result<(), McpError> {
        if !self.config.enabled {
            return Ok(());
        }
        // Defense in depth: `McpConfig::validate` already rejects duplicates.
        let mut seen: HashSet<&str> = HashSet::new();
        for server in self.config.servers.iter().filter(|s| s.enabled) {
            if !seen.insert(server.id.as_str()) {
                return Err(McpError::DuplicateServer { server_id: server.id.clone() });
            }
            if let Some(handle) =
                self.servers.read().unwrap_or_else(|e| e.into_inner()).get(&server.id).cloned()
            {
                // Connected on a previous run (or Failed/Stopped with zero
                // tools): re-bridge into this run's fresh registry. Same
                // collision handling as first registration — log loudly,
                // never clobber, never abort the run.
                match self.re_register_tools(&handle, registry) {
                    Ok(count) => {
                        tracing::debug!(server_id = %server.id, tool_count = count, "mcp server tools re-registered");
                    }
                    Err(McpError::DuplicateTool { name }) => {
                        tracing::error!(
                            server_id = %server.id,
                            tool = %name,
                            "mcp tool name collision during re-registration"
                        );
                    }
                    Err(e) => return Err(e),
                }
                continue;
            }
            match self.connect_server(server, registry).await {
                Ok(count) => {
                    tracing::info!(server_id = %server.id, tool_count = count, "mcp server connected");
                }
                Err(McpError::DuplicateTool { name }) => {
                    // Collision (ADR-43 §4): connect_server already rolled
                    // back this server's tools and marked it Failed. Log
                    // loudly and continue — startup must not fail because of
                    // one bad server.
                    tracing::error!(
                        server_id = %server.id,
                        tool = %name,
                        "mcp tool name collision; server marked failed"
                    );
                }
                Err(e) => {
                    // Failed state was already recorded on the client and the
                    // handle was registered; the event carries the detail.
                    tracing::warn!(server_id = %server.id, error = %e, "mcp server failed to start; continuing");
                }
            }
        }
        Ok(())
    }

    /// Re-bridge an already-connected server's tools into a fresh registry
    /// (the runtime builds one `ToolRegistry` per agent run). `Failed`/`Stopped`
    /// servers hold zero tools, so this is a no-op for them. Collision-checked
    /// exactly like first registration (ADR-43 §4).
    fn re_register_tools(
        &self,
        handle: &McpServerHandle,
        registry: &mut ToolRegistry,
    ) -> Result<usize, McpError> {
        let mut count = 0;
        for tool in handle.tools.read().unwrap_or_else(|e| e.into_inner()).iter() {
            let name = tool.tool_name();
            if registry.get(name).is_some() {
                return Err(McpError::DuplicateTool { name: name.to_string() });
            }
            registry.register(Box::new(tool.clone()));
            count += 1;
        }
        Ok(count)
    }

    /// Current lifecycle state of `server_id`, or [`McpServerState::Disabled`]
    /// when the server is not configured/registered.
    pub fn server_state(&self, server_id: &str) -> McpServerState {
        self.servers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id)
            .map(|handle| handle.state())
            .unwrap_or(McpServerState::Disabled)
    }

    /// Snapshot of every registered server: `(server_id, state, tool_count)`.
    /// Sorted by id for stable UI rendering.
    pub fn servers(&self) -> Vec<(String, McpServerState, usize)> {
        let mut list: Vec<_> = self
            .servers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, handle)| (id.clone(), handle.state(), handle.tool_count()))
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    /// The unqualified tool descriptors of `server_id`, for the Task 7 UI.
    pub fn tools_for(&self, server_id: &str) -> Vec<McpToolDescriptor> {
        self.servers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id)
            .map(|handle| handle.tool_descriptors())
            .unwrap_or_default()
    }

    /// Live-connect one configured server and register its tools (UI toggle).
    ///
    /// A server already registered is stopped first, so this is a clean
    /// reconnect. Returns the number of registered tools.
    pub async fn start_server(
        &self,
        server_id: &str,
        registry: &mut ToolRegistry,
    ) -> Result<usize, McpError> {
        if !self.config.enabled {
            return Err(McpError::ServerDisabled { server_id: server_id.to_string() });
        }
        let server = self
            .config
            .servers
            .iter()
            .find(|s| s.id == server_id)
            .ok_or_else(|| McpError::UnknownServer { server_id: server_id.to_string() })?;
        if !server.enabled {
            return Err(McpError::ServerDisabled { server_id: server_id.to_string() });
        }
        if self.servers.read().unwrap_or_else(|e| e.into_inner()).contains_key(server_id) {
            let _ = self.stop_server(server_id, registry).await;
        }
        self.connect_server(server, registry).await
    }

    /// Live-disconnect one server (UI toggle): stops its client, removes its
    /// tools from the registry, and leaves the handle registered with state
    /// `Stopped`. Idempotent for a server that is already stopped/failed.
    pub async fn stop_server(
        &self,
        server_id: &str,
        registry: &mut ToolRegistry,
    ) -> Result<(), McpError> {
        let handle = self
            .servers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer { server_id: server_id.to_string() })?;
        for name in handle.tool_names() {
            registry.unregister(&name);
        }
        handle.clear_tools();
        match handle.client().lock().await.stop().await {
            Ok(_) => {}
            // Never started (already Stopped / registration failed before
            // spawn): nothing to reap.
            Err(McpError::NotConnected) => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Gracefully stop every live server and remove its tools from `registry`.
    /// Errors are logged, never propagated: teardown must not fail the run.
    pub async fn stop_all(&self, registry: &mut ToolRegistry) {
        let ids: Vec<String> =
            self.servers.read().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
        for id in ids {
            if let Err(e) = self.stop_server(&id, registry).await {
                tracing::warn!(server_id = %id, error = %e, "mcp stop_server failed during stop_all");
            }
        }
    }

    /// Spawn + `initialize` + `tools/list` one server, collision-check its
    /// tools into `registry`, and register its handle. See [`Self::register_tools`]
    /// for the failure semantics.
    ///
    /// A handle (with zero tools) is registered **before** the handshake so
    /// the state watcher can reach it: a mid-flight crash clears the handle's
    /// tools (tools marked unavailable, ADR-43 §7) and the UI renders the
    /// server as `Failed` with the recorded detail. On any failure
    /// (spawn/`initialize`/`tools/list`, or a duplicate tool name) the client
    /// is marked `Failed` and the empty handle stays registered —
    /// `register_tools` then logs and continues.
    async fn connect_server(
        &self,
        server: &McpServerConfig,
        registry: &mut ToolRegistry,
    ) -> Result<usize, McpError> {
        let client = Arc::new(Mutex::new(McpClient::new(&server.id)));
        let state_rx = client.lock().await.subscribe_state();
        // Provisional handle registered up front so the watcher (spawned
        // below) can clear the server's tools when it later crashes. Tools are
        // filled in once registration succeeds.
        let handle = Arc::new(McpServerHandle {
            client: client.clone(),
            tools: RwLock::new(Vec::new()),
            state_rx: state_rx.clone(),
        });
        self.servers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server.id.clone(), handle.clone());
        // Watch the client's state channel and publish events for every
        // transition. Holds only `Weak` handles so a dropped manager is not
        // kept alive by the watcher.
        spawn_state_watcher(
            server.id.clone(),
            self.bus.clone(),
            Arc::downgrade(&client),
            Arc::downgrade(&handle),
            state_rx,
        );

        let timeout_secs = server.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let env: Vec<(&str, &str)> = server
            .env
            .as_ref()
            .map(|map| map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect())
            .unwrap_or_default();

        let descriptors = match self.handshake(server, &client, timeout_secs, &env).await {
            Ok(tools) => tools,
            Err((err, detail)) => {
                client.lock().await.record_failure(detail);
                return Err(err);
            }
        };
        // The server died mid-handshake (e.g. crash-on-start observed during
        // `tools/list`): register nothing and leave the empty Failed handle.
        if handle.state() != McpServerState::Connected {
            let detail = "server exited during handshake".to_string();
            client.lock().await.record_failure(detail);
            return Err(McpError::NotConnected);
        }

        // Collision-checked registration (ADR-43 §4): never silently clobber.
        let mut tools = Vec::with_capacity(descriptors.len());
        let mut registered_names: Vec<String> = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            let name = format!("mcp:{}:{}", server.id, descriptor.name);
            if registry.get(&name).is_some() {
                // Roll back this server's partially-registered tools, mark it
                // Failed, and fail loudly. Tools of *other* servers/plugins
                // are untouched; `register_tools` logs and continues.
                for registered in &registered_names {
                    registry.unregister(registered);
                }
                let detail = format!("tool name '{name}' already registered");
                client.lock().await.record_failure(detail);
                return Err(McpError::DuplicateTool { name });
            }
            let tool = McpTool::new(server.id.clone(), client.clone(), descriptor)
                .with_default_timeout_secs(timeout_secs);
            registry.register(Box::new(tool.clone()));
            registered_names.push(name);
            tools.push(tool);
        }

        handle.replace_tools(tools);
        Ok(registered_names.len())
    }

    /// Spawn, `initialize`, and list the tools of `client`'s server.
    /// Failures are returned as `(error, human-readable detail)` so the caller
    /// can record them on the client and mark the server `Failed`.
    async fn handshake(
        &self,
        server: &McpServerConfig,
        client: &Arc<Mutex<McpClient>>,
        timeout_secs: u64,
        env: &[(&str, &str)],
    ) -> Result<Vec<McpToolDescriptor>, (McpError, String)> {
        let mut guard = client.lock().await;
        if let Err(e) = guard.spawn(&server.command, &server.args, env).await {
            let detail = format!("spawn failed: {e}");
            return Err((e, detail));
        }
        if let Err(e) = guard.initialize(timeout_secs).await {
            let detail = format!("initialize failed: {e}");
            return Err((e, detail));
        }
        guard.list_tools(timeout_secs, CancellationToken::new()).await.map_err(|e| {
            let detail = format!("tools/list failed: {e}");
            (e, detail)
        })
    }
}

/// Publish [`EventKind::McpServerStateChanged`] for every transition on the
/// client's state watch channel. `Failed` transitions carry the client's last
/// failure detail and clear the server handle's tools (tools marked
/// unavailable, ADR-43 §7). Exits when the client (the channel sender) is
/// dropped.
fn spawn_state_watcher(
    server_id: String,
    bus: EventBus,
    client_weak: Weak<Mutex<McpClient>>,
    handle_weak: Weak<McpServerHandle>,
    mut rx: watch::Receiver<McpServerState>,
) {
    tokio::spawn(async move {
        loop {
            match rx.changed().await {
                Ok(()) => {}
                Err(_) => break, // sender dropped (client gone)
            }
            let state = *rx.borrow();
            let error = if state == McpServerState::Failed {
                if let Some(handle) = handle_weak.upgrade() {
                    handle.clear_tools();
                }
                match client_weak.upgrade() {
                    Some(client) => client.lock().await.last_failure_detail(),
                    None => None,
                }
            } else {
                None
            };
            let kind =
                EventKind::McpServerStateChanged { server_id: server_id.clone(), state, error };
            if let Err(e) = bus.publish_raw(kind) {
                tracing::warn!(server = %server_id, error = %e, "failed to publish mcp state event");
            }
        }
    });
}
