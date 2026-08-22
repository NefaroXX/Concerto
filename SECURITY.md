# Security Policy

Concerto executes model-generated actions on a real developer machine. Its
policy engine, virtual filesystem, shell validation, plugin limits, audit log,
and undo features reduce risk; they do **not** make generated code trusted or
provide a complete operating-system sandbox. The enforced boundaries, current
limitations, and the plugin threat model are documented in detail in
[SECURITY_BOUNDARIES.md](SECURITY_BOUNDARIES.md).

## Supported Versions

Concerto is pre-release software. Only the active development line receives
security fixes; there is no long-term-support branch.

| Version                          | Supported          |
| -------------------------------- | ------------------ |
| 0.1.x (pre-release, source builds on `main`/`dev`) | :white_check_mark: |
| older builds                     | :x:                |

Security fixes are applied to the active development branch and included in
the next test release. There is no stable supported release yet.

## Reporting a Vulnerability

**Do not open a public issue for a security vulnerability.**

Report privately through GitHub's vulnerability reporting:

1. Open the **Security** tab of this repository.
2. Click **Report a vulnerability**.
3. Include:
   - the affected commit hash or release;
   - a description of the impact;
   - reproduction steps and a minimal proof of concept where practical;
   - whether disclosure is time-sensitive.

No project email address is published; all vulnerability coordination happens
through GitHub's private channel. The maintainer will acknowledge the report,
assess severity, work with you on a fix or mitigation, and coordinate
disclosure after one is available. Reporters who wish to be credited will be
acknowledged.

### In scope

Examples of reportable security issues:

- Bypassing `SimplePolicyEngine` or `ToolExecutor` authorization for file
  writes, shell commands, git operations, or `mcp:*` tools.
- Escaping `VirtualFs` confinement, the `project_roots` allowlist, or session
  root checks in the API server.
- API server authentication bypasses on non-loopback binds.
- Secrets (API keys, keychain material) leaking into logs, config files, audit
  records, or event streams.
- WASM plugin escapes beyond the documented capability limits, or skills/MCP
  content granting undeclared capabilities.
- Credential-test mode (`CONCERTO_TEST_MODE=1`) being reachable in production
  paths.

### Out of scope

- Issues that require an already-compromised host or a malicious user running
  arbitrary code as you.
- Social engineering of end users.
- The documented limitations and gaps listed in
  [SECURITY_BOUNDARIES.md](SECURITY_BOUNDARIES.md) — stated limitations are
  known and are not vulnerabilities. In particular: policy gates are safety
  layers, not an OS sandbox; `SandboxProfile::Containerized` is not
  implemented.

## Non-security Bugs

For ordinary bugs without security impact, use the repository issue tracker
with the bug report template.
