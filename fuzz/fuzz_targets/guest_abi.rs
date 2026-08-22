//! Fuzz target for the guest ABI pack/unpack functions.
//!
//! Tests that the ABI packing and unpacking functions handle arbitrary
//! input without panicking or causing undefined behavior. This is critical
//! for plugin security since plugins can send arbitrary data through the ABI.

#![no_main]

use libfuzzer_sys::fuzz_target;
use concerto_plugins::guest_abi::{pack_ptr_len, unpack_ptr_len};

fuzz_target!(|data: &[u8]| {
    // Need at least 8 bytes for an i64
    if data.len() < 8 {
        return;
    }

    // Extract an i64 from the fuzz input
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[..8]);
    let packed = i64::from_le_bytes(bytes);

    // Unpack should never panic
    let (ptr, len) = unpack_ptr_len(packed);

    // Pack the unpacked values and verify round-trip consistency
    let repacked = pack_ptr_len(ptr, len);
    
    // The repacked value should equal the original (within the valid bit range)
    // Note: pack_ptr_len uses bitwise OR, so high bits may differ
    let (repacked_ptr, repacked_len) = unpack_ptr_len(repacked);
    assert_eq!(ptr, repacked_ptr, "ptr round-trip failed");
    assert_eq!(len, repacked_len, "len round-trip failed");
});
