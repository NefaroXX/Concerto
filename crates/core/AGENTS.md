# OVERVIEW
Core crate provides the foundational traits, error handling, event bus, policy engine and executor used throughout Concerto.

## STRUCTURE
```
core/
├── lib.rs                # public re-exports
├── error.rs              # error taxonomy
├── event.rs              # event bus implementation
├── policy.rs             # policy engine core
├── executor.rs           # tool executor abstraction
├── types.rs              # core type definitions
├── memory.rs             # memory-related traits
├── ids.rs                # strongly typed IDs
├── helpers.rs            # assorted helper functions
├── policy_presets.rs     # built-in policy presets
├── testing.rs            # test utilities
└── traits/               # foundational trait definitions
    ├── mod.rs             # module index
    ├── agent.rs           # agent trait
    ├── approval.rs        # approval/UI trait
    ├── memory.rs          # memory store trait
    ├── policy.rs          # policy engine trait
    ├── provider.rs        # LLM provider trait
    └── tool.rs            # tool executor trait
```

## WHERE TO LOOK
| Concern | File(s) |
|--------|---------|
| Public API | `lib.rs` |
| Error hierarchy | `error.rs` |
| Event propagation | `event.rs` |
| Policy evaluation | `policy.rs`, `policy_presets.rs` |
| Executor contract | `executor.rs` |
| Core types | `types.rs`, `ids.rs` |
| Memory abstractions | `memory.rs` |
| Helper utilities | `helpers.rs` |
| Trait definitions | `traits/` directory |
| Test helpers | `testing.rs` |

## CONVENTIONS
- **Edition** - Rust 2021, MSRV 1.88
- **Error handling** - public APIs use the domain error appropriate to the
  trait/module (`CoreError`, `ToolError`, `PolicyError`, and others); `anyhow` is
  reserved for application boundaries
- **Trait design** - traits are object-safe where needed, use associated types for flexibility, and are documented with `///` comments
- **Visibility** - only items needed outside the crate are `pub`; internal modules stay private
- **Naming** - snake_case for modules, PascalCase for types, UPPER_SNAKE for constants
- **Async** - `async_trait` is used for async trait methods; the
  `concerto_core::CancellationToken` re-export is passed to long-running work
- **Testing** - `testing.rs` provides `#[cfg(test)]` utilities; unit tests live beside the implementation they exercise

## ANTI-PATTERNS
- Avoid `unwrap` or `expect` in production code; use proper error conversion
- Do not expose internal structs directly; wrap them in public newtype if needed
- Never place `println!` in library code; diagnostics go through `tracing` or
  typed events/errors
- Do not make traits generic over lifetimes unless required; keep them simple
- Refrain from using `static mut`; prefer `Arc<Mutex<...>>` or `RwLock`
- Skip duplicate implementations; reuse default trait methods when possible
- Do not import the whole crate with `use crate::*`; import only needed symbols
