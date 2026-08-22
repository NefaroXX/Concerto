//! Guest ABI constants, helper types, and the scratch-buffer protocol.
//!
//! These are used by the host to interact with plugin WASM modules. The
//! plugin SDK crate mirrors this on the guest side.

/// Host ABI version for Phase 7.
pub const HOST_ABI_VERSION: u32 = 1;

/// Packed return: (ptr: i32, len: i32) stored as a single i64.
/// RESULT_ERROR (-1) indicates an error occurred.
pub const RESULT_ERROR: i64 = -1i64;

/// Encode a (ptr, len) pair into a single i64 return value.
pub fn pack_ptr_len(ptr: i32, len: i32) -> i64 {
    (ptr as i64) << 32 | (len as i64 & 0xFFFF_FFFF)
}

/// Decode an i64 into (ptr, len).
pub fn unpack_ptr_len(val: i64) -> (i32, i32) {
    let ptr = (val >> 32) as i32;
    let len = (val & 0xFFFF_FFFF) as i32;
    (ptr, len)
}

/// Default guest scratch buffer size: 64 KiB.
pub const DEFAULT_SCRATCH_SIZE: usize = 65536;

/// Maximum scratch buffer size: 256 MiB.
pub const MAX_SCRATCH_SIZE: usize = 256 * 1024 * 1024;

/// Expected WASM export name for provider dispatch.
pub const EXPORT_CALL_PROVIDER: &str = "call_provider";

/// Expected WASM export name for memory adapter dispatch.
pub const EXPORT_CALL_ADAPTER: &str = "call_adapter";

/// Expected WASM export name for dialect dispatch (ADR-53).
///
/// `call_dialect(op_ptr, op_len, input_ptr, input_len, scratch_ptr, scratch_len) -> i64`.
/// Defined ops: `"render"` and `"cache"`; any other op returns the error
/// `"unsupported operation"`.
pub const EXPORT_CALL_DIALECT: &str = "call_dialect";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_ptr_len_roundtrip() {
        let cases = [(0, 0), (1, 100), (65536, 4096), (0x7FFF_FFFF, 0xFFFF)];
        for &(ptr, len) in &cases {
            let packed = pack_ptr_len(ptr, len);
            let (decoded_ptr, decoded_len) = unpack_ptr_len(packed);
            assert_eq!(decoded_ptr, ptr, "ptr mismatch");
            assert_eq!(decoded_len, len, "len mismatch");
        }
    }

    #[test]
    fn result_error_is_negative_one() {
        assert_eq!(RESULT_ERROR, -1i64);
        let (ptr, len) = unpack_ptr_len(RESULT_ERROR);
        assert_eq!(ptr, -1);
        assert_eq!(len, -1);
    }

    #[test]
    fn default_scratch_size_is_64k() {
        assert_eq!(DEFAULT_SCRATCH_SIZE, 65536);
    }

    #[test]
    fn dialect_export_name_is_call_dialect() {
        assert_eq!(EXPORT_CALL_DIALECT, "call_dialect");
    }
}
