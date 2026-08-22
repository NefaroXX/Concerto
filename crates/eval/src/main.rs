//! The `cargo-eval` binary has moved to `crates/eval-runner/`.
//!
//! The `concerto-eval` crate is now a library only. To run evaluations:
//!
//!     cargo run -p concerto-eval-runner -- --suite=<path> [--config=<path>] [--baseline=<path>]
//!
//! This indirection avoids a circular dependency (`concerto-eval` -> `concerto-orchestrator` -> `concerto-eval`).

fn main() {
    eprintln!(
        "The cargo-eval binary has moved to the concerto-eval-runner crate.\n\
         Run: cargo run -p concerto-eval-runner -- [ARGS]"
    );
    std::process::exit(1);
}
