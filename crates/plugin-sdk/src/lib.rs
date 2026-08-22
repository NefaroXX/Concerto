#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]
// SAFETY INVARIANT — why `unsafe_code` is permitted in this crate (audit M-04).
//
// This crate is the *guest-side* SDK for Concerto WASM plugins. Its entire
// purpose is to provide the ABI bridge between a no_std WASM module and its
// host: reading tool/operation inputs out of WASM linear memory and writing
// results back into a host-provided scratch buffer. Every `unsafe` item in
// this file is part of that ABI and is load-bearing — removing them would
// eliminate the only mechanism by which a plugin can exchange data with the
// host. The workspace denies `unsafe_code` by policy; this crate is the
// sole, contained exception.
//
// Invariant — the four safety contracts every `unsafe` use relies on:
//
//   1. Single-threaded execution. WASM modules compiled with the standard
//      toolchain have no threads, so the `static mut scratch_buffer: i32`
//      globals written by the host and read by the plugin — one emitted in
//      each of the `plugin_entry!`, `plugin_entry_provider!`,
//      `plugin_entry_adapter!`, and `plugin_entry_dialect!` macros — incur
//      no data race. The host sets the scratch pointer once at
//      instantiation before dispatching any export.
//
//   2. Host-supplied linear-memory bounds. `read_linear` and the
//      `read_linear` calls emitted by the macros receive pointers/lengths the
//      host validated before the call. WASM additionally bounds-checks every
//      memory access at runtime, so an out-of-bounds host pointer traps
//      deterministically rather than corrupting memory. Negative/zero length
//      is rejected explicitly before any pointer arithmetic.
//
//   3. Scratch-buffer capacity. `write_scratch` refuses to write when
//      `bytes.len() > scratch_size`, returning `RESULT_ERROR` instead. The
//      host advertises capacity via `scratch_buffer_size` (DEFAULT 64 KiB)
//      and passes a per-call scratch region in `call_tool`/`call_provider`/
//      `call_adapter`; both originate from the same host allocation logic in
//      `crates/plugins/src/guest_abi.rs`, which must mirror these constants.
//
//   4. No aliasing of the result region with the input region. The host
//      allocates the scratch result buffer separately from the input slices
//      it passes, so `copy_nonoverlapping` in `write_scratch` does not
//      violate its `nonoverlapping` precondition. If a future host change
//      reuses one region for both, the `copy_nonoverlapping` precondition
//      would be violated — review `guest_abi.rs` together with any change
//      here.
//
// Any new `unsafe` addition to this crate MUST extend this invariant list.
// Consumers of the `plugin_entry!` / `plugin_entry_provider!` /
// `plugin_entry_adapter!` / `plugin_entry_dialect!` macros inherit this allow
// via the emitted `#[allow(unsafe_code)]` on `static mut scratch_buffer`; they
// are still bound by the contracts above.
#![allow(unsafe_code)]

//! Guest-side SDK for Concerto WASM plugins.
//!
//! Guests pick the entry-point macro matching their plugin kind:
//! [`plugin_entry!`] (tool), [`plugin_entry_provider!`] (provider),
//! [`plugin_entry_adapter!`] (memory adapter), or
//! [`plugin_entry_dialect!`] (wire dialect, ADR-53).
//!
//! # Usage
//!
//! ```ignore
//! use concerto_plugin_sdk::plugin_entry;
//!
//! fn my_manifest() -> &'static [u8] {
//! br#"{"name":"my-plugin","abi_version":1}"#
//! }
//! fn my_call_tool(name: &str, args: &str) -> String {
//! format!("called {name} with {args}")
//! }
//! plugin_entry!(my_manifest, my_call_tool);
//! ```
//!
//! Compile with `--target wasm32-wasip2`.
//!
//! # ABI Version 1 (Current)
//!
//! The plugin must export the following functions:
//! - `manifest() -> i64`: Returns a packed pointer/length of JSON manifest
//! - `call_tool(name_ptr: i32, name_len: i32, input_ptr: i32, input_len: i32, scratch_ptr: i32, scratch_len: i32) -> i64`: Calls a tool with input and returns result in scratch buffer
//! - `init() -> i32`: Initializes plugin (returns 0 on success)
//! - `scratch_buffer_size: i32`: Immutable global (64 KiB)
//! - `scratch_buffer: i32`: Mutable global (initialized by host)
//!
//! The `scratch_buffer` is used for `manifest()` output but `call_tool()` uses the scratch buffer passed as a parameter.

#![no_std]
#![forbid(unused_imports)]

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// ABI constants  (must match crates/plugins/src/guest_abi.rs)
// ---------------------------------------------------------------------------

/// Return value encoding: high 32 bits = pointer, low 32 bits = length.
pub const RESULT_ERROR: i64 = -1;

/// Successful return indicator.
pub const RESULT_SUCCESS: i64 = 0;

/// Current ABI version.
pub const HOST_ABI_VERSION: i32 = 1;

/// Default scratch-buffer size (64 KiB).
pub const DEFAULT_SCRATCH_SIZE: i32 = 65_536;

/// Expected WASM export name for provider dispatch.
pub const EXPORT_CALL_PROVIDER: &str = "call_provider";

/// Expected WASM export name for memory adapter dispatch.
pub const EXPORT_CALL_ADAPTER: &str = "call_adapter";

/// Expected WASM export name for dialect dispatch (ADR-53).
///
/// `call_dialect(op_ptr, op_len, input_ptr, input_len, scratch_ptr, scratch_len) -> i64`.
/// Mirrors [`EXPORT_CALL_PROVIDER`] in `crates/plugins/src/guest_abi.rs`.
pub const EXPORT_CALL_DIALECT: &str = "call_dialect";

// ---------------------------------------------------------------------------
// ABI helpers
// ---------------------------------------------------------------------------

/// Pack a linear-memory pointer and length into a single i64.
#[inline]
pub fn pack_ptr_len(ptr: i32, len: i32) -> i64 {
    ((ptr as i64) << 32) | (len as i64 & 0xFFFF_FFFF)
}

/// Unpack an i64 into a (pointer, length) pair.
#[inline]
pub fn unpack_ptr_len(val: i64) -> (i32, i32) {
    let ptr = (val >> 32) as i32;
    let len = (val & 0xFFFF_FFFF) as i32;
    (ptr, len)
}

// ---------------------------------------------------------------------------
// Scratch-buffer helpers (take pointer via parameter)
// ---------------------------------------------------------------------------

/// Write `bytes` into the scratch buffer at `scratch_ptr` and return a
/// packed i64.  Returns `RESULT_ERROR` if data exceeds `scratch_size`.
///
/// # Safety
///
/// `scratch_ptr` must point to a valid region of at least `scratch_size`
/// bytes in WASM linear memory.
pub unsafe fn write_scratch(bytes: &[u8], scratch_ptr: *mut u8, scratch_size: usize) -> i64 {
    let len = bytes.len();
    if len > scratch_size {
        return RESULT_ERROR;
    }
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), scratch_ptr, len);
    // Return ptr so the host knows where the result lives.
    pack_ptr_len(scratch_ptr as i32, len as i32)
}

/// Read `len` bytes from the scratch buffer at `scratch_ptr`.
///
/// Returns an empty `Vec` on overflow.
///
/// # Safety
///
/// `scratch_ptr` must point to a valid region of at least `scratch_size`
/// bytes in WASM linear memory.
pub unsafe fn read_scratch(len: usize, scratch_ptr: *mut u8, scratch_size: usize) -> Vec<u8> {
    if len > scratch_size {
        return Vec::new();
    }
    let slice = core::ptr::slice_from_raw_parts(scratch_ptr, len);
    (*slice).to_owned()
}

// ---------------------------------------------------------------------------
// Entry-point macro
// ---------------------------------------------------------------------------

/// Generate the WASM exports for a plugin.
///
/// The macro expects:
/// 1. A nullary function returning `&'static [u8]` (manifest JSON).
/// 2. A binary function `fn(&str, &str) -> String` that dispatches tool calls.
///
/// Generates exports: `manifest`, `call_tool` (canonical ABI v1, 6-param),
/// `init`, `scratch_buffer` (mutable global), and `scratch_buffer_size`
/// (immutable global).
///
/// # ABI v1 — `call_tool` signature
///
/// ```text
/// call_tool(
///     name_ptr:  i32,   // pointer to tool name in linear memory
///     name_len:  i32,   // length of tool name
///     input_ptr: i32,   // pointer to JSON input in linear memory
///     input_len: i32,   // length of JSON input
///     scratch_ptr: i32, // pointer to scratch buffer for result output
///     scratch_len: i32, // capacity of scratch buffer
/// ) -> i64             // packed (ptr, len) or RESULT_ERROR
/// ```
#[macro_export]
macro_rules! plugin_entry {
    ($manifest_fn:ident, $call_tool_fn:ident) => {
        // Own the scratch-buffer globals here so no cross-crate static mut
        // access is needed.
        #[no_mangle]
        pub static scratch_buffer_size: i32 = $crate::DEFAULT_SCRATCH_SIZE;

        // Written once by the host at instantiation to tell us where the
        // scratch buffer lives in linear memory.
        #[allow(unsafe_code)]
        #[no_mangle]
        pub static mut scratch_buffer: i32 = 0;

        fn do_manifest() -> i64 {
            let bytes = $manifest_fn();
            // SAFETY: scratch_buffer is initialised by the host before the
            // first call.  Single-threaded WASM execution.
            unsafe {
                let ptr = scratch_buffer as *mut u8;
                let size = scratch_buffer_size as usize;
                $crate::write_scratch(bytes, ptr, size)
            }
        }

        /// Canonical ABI v1 call_tool — 6 parameters.
        fn do_call_tool(
            name_ptr: i32,
            name_len: i32,
            arg_ptr: i32,
            arg_len: i32,
            scratch_ptr: i32,
            scratch_len: i32,
        ) -> i64 {
            // SAFETY: the host provides valid linear-memory addresses.
            let name_bytes = unsafe { $crate::read_linear(name_ptr, name_len) };
            let name = match core::str::from_utf8(&name_bytes) {
                Ok(s) => s,
                Err(_) => return $crate::RESULT_ERROR,
            };

            let arg_bytes = unsafe { $crate::read_linear(arg_ptr, arg_len) };
            let args = match core::str::from_utf8(&arg_bytes) {
                Ok(s) => s,
                Err(_) => return $crate::RESULT_ERROR,
            };

            let result = $call_tool_fn(name, args);
            // SAFETY: scratch_ptr/scratch_len are provided by the host and
            // point to a valid scratch region in linear memory.
            unsafe {
                let ptr = scratch_ptr as *mut u8;
                let size = scratch_len as usize;
                $crate::write_scratch(result.as_bytes(), ptr, size)
            }
        }

        #[export_name = "manifest"]
        pub extern "C" fn __plugin_manifest_export() -> i64 {
            do_manifest()
        }

        /// Canonical ABI v1 call_tool export — 6 parameters.
        #[export_name = "call_tool"]
        pub extern "C" fn __plugin_call_tool_export(
            name_ptr: i32,
            name_len: i32,
            arg_ptr: i32,
            arg_len: i32,
            scratch_ptr: i32,
            scratch_len: i32,
        ) -> i64 {
            do_call_tool(name_ptr, name_len, arg_ptr, arg_len, scratch_ptr, scratch_len)
        }

        /// Default no-op init — returns 0 (success).
        #[export_name = "init"]
        pub extern "C" fn __plugin_init_export() -> i32 {
            0
        }
    };
}

// ---------------------------------------------------------------------------
// plugin_entry_provider! — for provider plugins
// ---------------------------------------------------------------------------

/// Generate the WASM exports for a **provider** plugin.
///
/// The macro expects:
/// 1. A nullary function returning `&'static [u8]` (manifest JSON).
/// 2. A binary function `fn(&str, &str) -> String` that dispatches provider
///    operations (e.g. `"complete"`, `"list_models"`).  The first argument is
///    the operation name, the second is the JSON input string, and the return
///    value is the JSON result string.
///
/// Generates exports: `manifest`, `call_provider`, `init`, `scratch_buffer`,
/// and `scratch_buffer_size`.
///
/// # ABI v1 — `call_provider` signature
///
/// ```text
/// call_provider(
///     op_ptr:      i32,   // pointer to operation name in linear memory
///     op_len:      i32,   // length of operation name
///     input_ptr:   i32,   // pointer to JSON input in linear memory
///     input_len:   i32,   // length of JSON input
///     scratch_ptr: i32,   // pointer to scratch buffer for result output
///     scratch_len: i32,   // capacity of scratch buffer
/// ) -> i64               // packed (ptr, len) or RESULT_ERROR
/// ```
#[macro_export]
macro_rules! plugin_entry_provider {
    ($manifest_fn:ident, $call_op_fn:ident) => {
        #[no_mangle]
        pub static scratch_buffer_size: i32 = $crate::DEFAULT_SCRATCH_SIZE;

        #[allow(unsafe_code)]
        #[no_mangle]
        pub static mut scratch_buffer: i32 = 0;

        fn do_manifest() -> i64 {
            let bytes = $manifest_fn();
            unsafe {
                let ptr = scratch_buffer as *mut u8;
                let size = scratch_buffer_size as usize;
                $crate::write_scratch(bytes, ptr, size)
            }
        }

        /// Canonical ABI v1 call_provider — 6 parameters.
        fn do_call_provider(
            op_ptr: i32,
            op_len: i32,
            input_ptr: i32,
            input_len: i32,
            scratch_ptr: i32,
            scratch_len: i32,
        ) -> i64 {
            let op_bytes = unsafe { $crate::read_linear(op_ptr, op_len) };
            let op = match core::str::from_utf8(&op_bytes) {
                Ok(s) => s,
                Err(_) => return $crate::RESULT_ERROR,
            };

            let input_bytes = unsafe { $crate::read_linear(input_ptr, input_len) };
            let input = match core::str::from_utf8(&input_bytes) {
                Ok(s) => s,
                Err(_) => return $crate::RESULT_ERROR,
            };

            let result = $call_op_fn(op, input);
            unsafe {
                let ptr = scratch_ptr as *mut u8;
                let size = scratch_len as usize;
                $crate::write_scratch(result.as_bytes(), ptr, size)
            }
        }

        #[export_name = "manifest"]
        pub extern "C" fn __plugin_provider_manifest_export() -> i64 {
            do_manifest()
        }

        #[export_name = "call_provider"]
        pub extern "C" fn __plugin_call_provider_export(
            op_ptr: i32,
            op_len: i32,
            input_ptr: i32,
            input_len: i32,
            scratch_ptr: i32,
            scratch_len: i32,
        ) -> i64 {
            do_call_provider(op_ptr, op_len, input_ptr, input_len, scratch_ptr, scratch_len)
        }

        #[export_name = "init"]
        pub extern "C" fn __plugin_provider_init_export() -> i32 {
            0
        }
    };
}

// ---------------------------------------------------------------------------
// plugin_entry_adapter! — for memory-adapter plugins
// ---------------------------------------------------------------------------

/// Generate the WASM exports for a **memory adapter** plugin.
///
/// The macro expects:
/// 1. A nullary function returning `&'static [u8]` (manifest JSON).
/// 2. A binary function `fn(&str, &str) -> String` that dispatches memory
///    adapter operations (e.g. `"store"`, `"search"`, `"list"`,
///    `"tombstone"`, `"delete_tombstoned"`, `"mark_stale"`,
///    `"delete_by_project"`, `"delete_by_file_path"`).  The first argument is
///    the operation name, the second is the JSON input string, and the return
///    value is the JSON result string.
///
/// Generates exports: `manifest`, `call_adapter`, `init`, `scratch_buffer`,
/// and `scratch_buffer_size`.
///
/// # ABI v1 — `call_adapter` signature
///
/// ```text
/// call_adapter(
///     op_ptr:      i32,   // pointer to operation name in linear memory
///     op_len:      i32,   // length of operation name
///     input_ptr:   i32,   // pointer to JSON input in linear memory
///     input_len:   i32,   // length of JSON input
///     scratch_ptr: i32,   // pointer to scratch buffer for result output
///     scratch_len: i32,   // capacity of scratch buffer
/// ) -> i64               // packed (ptr, len) or RESULT_ERROR
/// ```
#[macro_export]
macro_rules! plugin_entry_adapter {
    ($manifest_fn:ident, $call_op_fn:ident) => {
        #[no_mangle]
        pub static scratch_buffer_size: i32 = $crate::DEFAULT_SCRATCH_SIZE;

        #[allow(unsafe_code)]
        #[no_mangle]
        pub static mut scratch_buffer: i32 = 0;

        fn do_manifest() -> i64 {
            let bytes = $manifest_fn();
            unsafe {
                let ptr = scratch_buffer as *mut u8;
                let size = scratch_buffer_size as usize;
                $crate::write_scratch(bytes, ptr, size)
            }
        }

        /// Canonical ABI v1 call_adapter — 6 parameters.
        fn do_call_adapter(
            op_ptr: i32,
            op_len: i32,
            input_ptr: i32,
            input_len: i32,
            scratch_ptr: i32,
            scratch_len: i32,
        ) -> i64 {
            let op_bytes = unsafe { $crate::read_linear(op_ptr, op_len) };
            let op = match core::str::from_utf8(&op_bytes) {
                Ok(s) => s,
                Err(_) => return $crate::RESULT_ERROR,
            };

            let input_bytes = unsafe { $crate::read_linear(input_ptr, input_len) };
            let input = match core::str::from_utf8(&input_bytes) {
                Ok(s) => s,
                Err(_) => return $crate::RESULT_ERROR,
            };

            let result = $call_op_fn(op, input);
            unsafe {
                let ptr = scratch_ptr as *mut u8;
                let size = scratch_len as usize;
                $crate::write_scratch(result.as_bytes(), ptr, size)
            }
        }

        #[export_name = "manifest"]
        pub extern "C" fn __plugin_adapter_manifest_export() -> i64 {
            do_manifest()
        }

        #[export_name = "call_adapter"]
        pub extern "C" fn __plugin_call_adapter_export(
            op_ptr: i32,
            op_len: i32,
            input_ptr: i32,
            input_len: i32,
            scratch_ptr: i32,
            scratch_len: i32,
        ) -> i64 {
            do_call_adapter(op_ptr, op_len, input_ptr, input_len, scratch_ptr, scratch_len)
        }

        #[export_name = "init"]
        pub extern "C" fn __plugin_adapter_init_export() -> i32 {
            0
        }
    };
}

// ---------------------------------------------------------------------------
// plugin_entry_dialect! — for dialect plugins
// ---------------------------------------------------------------------------

/// Generate the WASM exports for a **dialect** plugin (ADR-53).
///
/// A dialect plugin owns the **request-side wire format** of the provider it
/// backs: it re-renders the canonical OpenAI-shaped request body into its own
/// wire dialect and applies cache-control semantics. It is a pure string →
/// string transformer — it makes no host calls.
///
/// The macro expects:
/// 1. A nullary function returning `&'static [u8]` (manifest JSON).
/// 2. A binary function `fn(&str, &str) -> String` that dispatches the two
///    dialect operations (`"render"` and `"cache"`; ADR-53 §1).
///    - `"render"` — input is the canonical envelope the host builds in
///      `DialectHost::render_chat_body` (`{"request": <canonical body>,
///      "model": "<model>", "echo": "always"|"if-present"}`); the plugin
///      returns the wire body for its dialect.
///    - `"cache"` — input `{"body": "<wire body>"}`; the plugin returns the
///      wire body with its cache-control semantics applied, or the body
///      unchanged for dialects without caching.
///      Any other op MUST return the error `{"error":"unsupported operation"}`
///      (see ADR-53 §2, and the host-facing [`crate::dialect_host`]).
///
/// Generates exports: `manifest`, `call_dialect`, `init`, `scratch_buffer`,
/// and `scratch_buffer_size`.
///
/// # ABI v1 — `call_dialect` signature
///
/// ```text
/// call_dialect(
///     op_ptr:      i32,   // pointer to operation name ("render"|"cache")
///     op_len:      i32,   // length of operation name
///     input_ptr:   i32,   // pointer to JSON input in linear memory
///     input_len:   i32,   // length of JSON input
///     scratch_ptr: i32,   // pointer to scratch buffer for result output
///     scratch_len: i32,   // capacity of scratch buffer
/// ) -> i64               // packed (ptr, len) or RESULT_ERROR
/// ```
#[macro_export]
macro_rules! plugin_entry_dialect {
    ($manifest_fn:ident, $call_op_fn:ident) => {
        #[no_mangle]
        pub static scratch_buffer_size: i32 = $crate::DEFAULT_SCRATCH_SIZE;

        #[allow(unsafe_code)]
        #[no_mangle]
        pub static mut scratch_buffer: i32 = 0;

        fn do_manifest() -> i64 {
            let bytes = $manifest_fn();
            unsafe {
                let ptr = scratch_buffer as *mut u8;
                let size = scratch_buffer_size as usize;
                $crate::write_scratch(bytes, ptr, size)
            }
        }

        /// Canonical ABI v1 call_dialect — 6 parameters.
        fn do_call_dialect(
            op_ptr: i32,
            op_len: i32,
            input_ptr: i32,
            input_len: i32,
            scratch_ptr: i32,
            scratch_len: i32,
        ) -> i64 {
            let op_bytes = unsafe { $crate::read_linear(op_ptr, op_len) };
            let op = match core::str::from_utf8(&op_bytes) {
                Ok(s) => s,
                Err(_) => return $crate::RESULT_ERROR,
            };

            let input_bytes = unsafe { $crate::read_linear(input_ptr, input_len) };
            let input = match core::str::from_utf8(&input_bytes) {
                Ok(s) => s,
                Err(_) => return $crate::RESULT_ERROR,
            };

            let result = $call_op_fn(op, input);
            unsafe {
                let ptr = scratch_ptr as *mut u8;
                let size = scratch_len as usize;
                $crate::write_scratch(result.as_bytes(), ptr, size)
            }
        }

        #[export_name = "manifest"]
        pub extern "C" fn __plugin_dialect_manifest_export() -> i64 {
            do_manifest()
        }

        #[export_name = "call_dialect"]
        pub extern "C" fn __plugin_call_dialect_export(
            op_ptr: i32,
            op_len: i32,
            input_ptr: i32,
            input_len: i32,
            scratch_ptr: i32,
            scratch_len: i32,
        ) -> i64 {
            do_call_dialect(op_ptr, op_len, input_ptr, input_len, scratch_ptr, scratch_len)
        }

        #[export_name = "init"]
        pub extern "C" fn __plugin_dialect_init_export() -> i32 {
            0
        }
    };
}

// ---------------------------------------------------------------------------
// Linear-memory read helper
// ---------------------------------------------------------------------------

/// Read `len` bytes from linear-memory address `ptr`.
///
/// # Safety
///
/// The caller must ensure the region `[ptr, ptr+len)` is valid in WASM
/// linear memory.  The WASM runtime guarantees bounds checking, but the
/// region must belong to the module's memory.
pub unsafe fn read_linear(ptr: i32, len: i32) -> Vec<u8> {
    if len <= 0 || ptr < 0 {
        return Vec::new();
    }
    let len = len as usize;
    let ptr = ptr as usize;
    let slice = core::ptr::slice_from_raw_parts(ptr as *const u8, len);
    (*slice).to_owned()
}
