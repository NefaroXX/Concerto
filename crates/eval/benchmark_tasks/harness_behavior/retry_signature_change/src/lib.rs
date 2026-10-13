//! Port parsing that panics on bad input. The task is to change the signature
//! to return a `Result`; the in-tree test already asserts the target behavior,
//! so an edit that keeps the panicking `u16` return does not compile.

pub fn parse_port(s: &str) -> u16 {
    s.parse().unwrap()
}
