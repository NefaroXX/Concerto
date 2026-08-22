use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PluginError {
    #[error("WASM runtime error: {0}")]
    Runtime(#[from] wasmtime::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("plugin manifest missing or invalid: {0}")]
    InvalidManifest(String),

    #[error("ABI version too new: plugin v{found}, host max v{max}")]
    AbiTooNew { found: u32, max: u32 },

    #[error("sidecar manifest mismatch")]
    ManifestMismatch,

    #[error("plugin init failed with code {0}")]
    InitFailed(i32),

    #[error("memory violation: ptr={ptr}, len={len}")]
    MemoryViolation { ptr: i32, len: i32 },

    #[error("WASM module has no exported memory")]
    NoMemory,

    #[error("invalid UTF-8 in plugin string")]
    InvalidUtf8,

    #[error("host I/O error")]
    HostIo,

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("shell execution error: {0}")]
    ShellExecution(String),

    #[error("scratch buffer not exported by plugin")]
    MissingScratchBuffer,

    #[error("scratch buffer too small: {size} bytes, minimum 64KB")]
    ScratchTooSmall { size: i32 },

    #[error("capability denied: {0}")]
    CapabilityDenied(String),

    #[error("memory grow failed")]
    MemoryGrow,

    #[error("memory write failed: {0}")]
    MemoryWrite(wasmtime::MemoryAccessError),

    #[error("plugin not found: {0}")]
    PluginNotFound(String),

    #[error("tool call failed: {0}")]
    ToolCallFailed(String),

    #[error("plugin provider call failed: {0}")]
    PluginProviderFailed(String),

    #[error("plugin dialect call failed: {0}")]
    PluginDialectFailed(String),

    #[error("plugin memory adapter call failed: {0}")]
    PluginAdapterFailed(String),

    #[error("plugin {id} is not active")]
    NotActive { id: String },

    #[error("plugin {plugin_id} made unauthorized host call to {capability}")]
    UnauthorizedHostCall { plugin_id: String, capability: String },

    #[error("plugin memory limit exceeded: current={current} bytes, max={max} bytes")]
    MemoryLimitExceeded { current: usize, max: usize },

    #[error(transparent)]
    Core(#[from] concerto_core::error::CoreError),

    #[error(transparent)]
    Tool(#[from] concerto_core::error::ToolError),
}
