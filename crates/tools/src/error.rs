//! Tool-local error helpers. Re-exports `ToolError` from `concerto-core`
//! so tool implementations only need `use crate::error::ToolError`.

pub use concerto_core::ToolError;
