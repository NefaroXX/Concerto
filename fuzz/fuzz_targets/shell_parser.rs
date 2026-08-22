//! Fuzz target for the shell parser.
//!
//! Tests that the shell parser handles arbitrary input without panicking,
//! crashing, or entering infinite loops. This is critical for security
//! since shell commands come from AI model outputs which may be malformed.

#![no_main]

use libfuzzer_sys::fuzz_target;
use concerto_shell::parser::parse_command;

fuzz_target!(|data: &[u8]| {
    // Convert fuzz input to a string (skip invalid UTF-8)
    let input = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return, // Skip invalid UTF-8
    };

    // The parser should handle any input without panicking
    let _ = parse_command(input);
});
