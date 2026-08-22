# ADR-25: Derive tool input JSON Schemas from Rust types

**Status:** Accepted
**Date:** 2026-07-15
**Deciders:** Concerto architecture

## Context

Concerto tools advertise their input contract to LLM providers as a JSON
Schema (`Tool::input_schema`) while deserializing model-supplied arguments
into a Rust struct via `serde`. These two representations of the same
contract were maintained by hand and drifted: the `shell` tool's
`ShellInput` required `args: Vec<String>` (no `#[serde(default)]`), but its
`input_schema` declared only `command` as required. A model that correctly
omitted `args` for an argument-less command (`pwd`, `ls`) was rejected with
`invalid shell input: missing field \`args\``, surfacing as an
`AgentLoopError`.

Hand-written dual contracts cannot be guaranteed consistent; any field
rename, type change, added `Option`, or requiredness change must be mirrored
in both places or the bug recurs.

## Decision

Tool input schemas are derived from the Rust input struct using `schemars`,
so the struct is the single source of truth. For the `shell` tool:

- `ShellInput` derives `JsonSchema`; each field carries a
  `#[schemars(description = "…")]`.
- `input_schema()` returns `schemars::schema_for!(ShellInput)`, sanitised to
  drop dialect/definition keywords (`$schema`, `$defs`, `definitions`) that
  some tool-calling APIs reject.
- Requiredness is driven by the struct: a field is required unless it is
  `Option<T>` or carries `#[serde(default)]`. `command` is required; `args`
  (with `#[serde(default)]`), `cwd`, and `timeout_secs` (`Option`) are
  optional.

Two test categories guard the contract (added in
`crates/tools/src/shell.rs`):

1. **Schema-shape** — `command` required, others optional, expected
   properties present, no provider-incompatible keywords emitted.
2. **Schema/runtime contract** — build the minimal input from the schema's
   own `required` set and assert it deserializes into `ShellInput` with
   correct defaults; also assert a fully-populated input deserializes.

This pattern is the reference for any future struct-based tool input.

## Consequences

- The advertised schema and the deserialization target cannot drift for
  schemars-derived tools; field names, types, nullability, and requiredness
  stay in lockstep.
- New dependency `schemars` (MIT) on `concerto-tools`.
- Residual risk remains where the struct is not authoritative: custom
  deserializers, post-deserialization validation, field aliases, flattened
  fields, or schema overrides can still diverge. Such cases must be covered
  by explicit contract tests.
- Other tools (git, filesystem, lsp, plugin) still parse arguments
  imperatively and remain out of scope; they should adopt this pattern when
  they move to struct-based inputs.
