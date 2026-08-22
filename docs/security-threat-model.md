# Concerto Threat Model

This document provides a comprehensive threat model for the Concerto AI coding agent, identifying assets, threat actors, attack vectors, and current mitigations. It builds on the security boundaries documented in [SECURITY_BOUNDARIES.md](../SECURITY_BOUNDARIES.md).

## Table of Contents

1. [Overview](#overview)
2. [Assets](#assets)
3. [Threat Actors](#threat-actors)
4. [Attack Vectors](#attack-vectors)
5. [Current Mitigations](#current-mitigations)
6. [Security Gaps](#security-gaps)
7. [Secure Development Guidelines](#secure-development-guidelines)
8. [Incident Response](#incident-response)

## Overview

Concerto is a local-first AI coding agent that executes model-generated actions on developer machines. It operates with the user's filesystem permissions and can interact with external LLM providers. The threat model addresses risks from:

- Malicious or compromised AI models
- Compromised plugins
- Credential exposure
- Command injection
- File system attacks
- Network-based attacks

**Trust Boundary**: Concerto runs in a trusted environment (developer machine) but processes untrusted inputs (AI model outputs, plugin code, user-provided files).

## Assets

### High-Value Assets

| Asset | Description | Impact if Compromised |
|-------|-------------|----------------------|
| **API Keys & Secrets** | Provider credentials (OpenAI, Anthropic, etc.), SSH keys, tokens | Financial loss, unauthorized API usage, account compromise |
| **Source Code** | Project files, proprietary code, intellectual property | IP theft, competitive advantage loss, compliance violations |
| **Shell Access** | Ability to execute arbitrary commands | Full system compromise, data exfiltration, lateral movement |
| **File System** | Read/write access to project and system files | Data theft, malware installation, configuration tampering |
| **Session Data** | Conversation history, tool outputs, memory embeddings | Context leakage, privacy violations, social engineering |
| **Audit Logs** | Records of all tool executions and decisions | Reconnaissance, compliance issues, privacy concerns |

### Medium-Value Assets

| Asset | Description | Impact if Compromised |
|-------|-------------|----------------------|
| **Configuration** | Policy rules, provider settings, plugin manifests | Policy bypass, unauthorized access, misconfiguration |
| **Memory Database** | SQLite FTS5/vector store with code embeddings | Code reconstruction, pattern analysis |
| **Plugin Binaries** | WASM modules with host function access | Code execution, sandbox escape |

## Threat Actors

### 1. Compromised AI Model

**Capability**: Generate malicious code, commands, or file operations  
**Motivation**: Adversarial training, prompt injection, model compromise  
**Access**: Indirect through generated outputs

**Attack Scenarios**:
- Generate shell commands with injection payloads
- Create files with path traversal attempts
- Output code with embedded secrets or backdoors
- Craft prompts to extract sensitive context from memory

### 2. Malicious Plugin Developer

**Capability**: Write WASM plugins with host function access  
**Motivation**: Data exfiltration, system compromise, supply chain attack  
**Access**: Direct through plugin execution

**Attack Scenarios**:
- Exploit host functions to access unauthorized resources
- Exfiltrate data through HTTP requests
- Consume excessive resources (CPU, memory, fuel)
- Escape WASM sandbox through runtime vulnerabilities

### 3. Supply Chain Attacker

**Capability**: Compromise dependencies or build artifacts  
**Motivation**: Widespread compromise, persistence  
**Access**: Indirect through dependency tree

**Attack Scenarios**:
- Inject malicious code into transitive dependencies
- Compromise build pipeline to modify binaries
- Exploit vulnerabilities in unmaintained dependencies

### 4. Local Adversary

**Capability**: Access to the developer machine  
**Motivation**: Data theft, privilege escalation, persistence  
**Access**: Direct filesystem and process access

**Attack Scenarios**:
- Read configuration files containing secrets
- Modify policy rules to bypass restrictions
- Intercept API keys from environment or keychain
- Tamper with plugin binaries on disk

### 5. Network Attacker

**Capability**: Intercept or manipulate network traffic  
**Motivation**: Credential theft, data exfiltration, MITM attacks  
**Access**: Network layer

**Attack Scenarios**:
- Intercept API keys in transit to providers
- Inject malicious responses from LLM providers
- Exploit unencrypted connections (if configured)
- Attack the API server if exposed beyond localhost

## Attack Vectors

### 1. Command Injection

**Vector**: Shell tool execution  
**Entry Point**: AI-generated commands  
**Current Mitigation**:
- Structured parsing with `bypass_shell: true` (no shell interpretation)
- Shell-wrap quoting with POSIX single-quote escape (Unix) or double-quote (Windows)
- Pre-execution denylist matched against raw (unquoted) string
- Allowlist matched against shell-quoted string
- Command validation before execution

**Residual Risk**:
- Windows cmd quoting is weaker than POSIX
- Permitted build tools can execute arbitrary behavior
- Shell validation is not a complete sandbox

### 2. Path Traversal

**Vector**: Filesystem tool operations  
**Entry Point**: AI-generated file paths  
**Current Mitigation**:
- `FilesystemTool` resolves paths under project root
- Root/path validation rejects traversal attempts
- `VirtualFs` stages changes before commit

**Residual Risk**:
- Shell commands can bypass filesystem overlay
- OS ACLs and external processes can race files
- Virtual staging is not an independent backup

### 3. Credential Exposure

**Vector**: Configuration files, environment variables, audit logs  
**Entry Point**: Misconfiguration, logging, memory dumps  
**Current Mitigation**:
- Provider secrets stored in OS keychain (not config files)
- Env-backed test stores (`CredentialStore::from_env`) limited to test processes
- Audit logs do not include secret values

**Residual Risk**:
- Environment variables visible to child processes
- Memory dumps may contain secrets
- Keychain access requires OS account compromise

### 4. Plugin Sandbox Escape

**Vector**: WASM plugin execution  
**Entry Point**: Malicious plugin code  
**Current Mitigation**:
- Module-size checks
- Fuel/time/resource limits
- Manifest capabilities
- Host-function mediation

**Residual Risk**:
- Not full OS-level isolation
- `SandboxProfile::Containerized` not implemented
- Runtime vulnerabilities can cross boundary
- `completion` host function not connected to LLM

### 5. Prompt Injection

**Vector**: AI model inputs  
**Entry Point**: User-provided files, memory context  
**Current Mitigation**:
- Policy rules evaluated before execution
- Approval required for sensitive operations
- Audit trail for all tool executions

**Residual Risk**:
- Model may be tricked into generating malicious outputs
- Memory context may contain injected prompts
- Policy rules cannot inspect model reasoning

### 6. API Key Leakage

**Vector**: Network traffic, logs, error messages  
**Entry Point**: Provider API calls, observability exports  
**Current Mitigation**:
- HTTPS for all provider connections
- API keys in keychain, not config files
- Observability exporters do not include secrets

**Residual Risk**:
- Error messages may include partial secrets
- Network interception if HTTPS misconfigured
- Observability endpoints may leak metadata

### 7. Denial of Service

**Vector**: Resource exhaustion  
**Entry Point**: Malicious commands, plugins, or model outputs  
**Current Mitigation**:
- Shell timeout clamping
- stdout/stderr caps
- Plugin fuel/memory limits
- Spend caps per session/task/day

**Residual Risk**:
- CPU-intensive operations not rate-limited
- Network requests not bounded
- Memory allocation not tracked for shell commands

## Current Mitigations

### Policy Engine

**Component**: `SimplePolicyEngine`  
**Strengths**:
- First-match-wins rule evaluation
- Deny-by-default for unmatched actions
- Structured conditions (tool name, path glob, command pattern, capability)
- Spend tracking and rate limiting
- Time-window auto-approval for trusted patterns

**Limitations**:
- Rules evaluated after model generation (cannot prevent generation)
- Command patterns cannot inspect transitive actions
- Allow-all mode bypasses policy (desktop expert mode)

### Virtual Filesystem

**Component**: `VirtualFs`  
**Strengths**:
- 4-entry state machine (Clean → Modified → Staged → Committed)
- Snapshot/restore for undo
- Hunk rejection for partial commits
- Root validation prevents escape

**Limitations**:
- Shell commands can bypass overlay
- Not an independent backup
- External processes can race files

### Shell Validation

**Component**: `ShellTool` with canonical profiles  
**Strengths**:
- Hard denylist consulted before allow-all mode
- Structured parsing avoids shell interpretation
- Shell-wrap quoting prevents metacharacter injection
- Timeout and output caps

**Limitations**:
- Windows cmd quoting weaker than POSIX
- Permitted tools can execute arbitrary behavior
- Not a container sandbox

### Plugin Sandbox

**Component**: WASM runtime (wasmtime)  
**Strengths**:
- Module-size checks
- Fuel limits (computation budget)
- Time limits (execution timeout)
- Resource limits (memory, instances)
- Manifest capabilities (declarative permissions)
- Host-function mediation (controlled API surface)

**Limitations**:
- Not full OS-level isolation
- Containerized profile not implemented
- Runtime vulnerabilities can escape
- Host functions are trusted code

### Audit Trail

**Component**: `AuditLog` with SQLite persistence  
**Strengths**:
- Records all tool executions
- Includes policy verdicts
- Tracks spend and latency
- Session-scoped for replay

**Limitations**:
- Does not prove absence of uninstrumented actions
- May contain sensitive metadata (paths, tool names)
- Not encrypted at rest

### API Server Authentication

**Component**: Bearer token middleware  
**Strengths**:
- Non-loopback bind requires API key
- `/v1/health` public, other routes authenticated
- Request body size limit (1 MiB)
- OpenAPI docs disabled by default

**Limitations**:
- API key stored in environment variable
- No rate limiting per client
- No IP allowlisting

## Security Gaps

### Critical Gaps

1. **No Containerized Plugin Sandbox**
   - **Risk**: WASM plugins can access host resources through granted capabilities
   - **Impact**: Data exfiltration, system compromise
   - **Mitigation**: Implement `SandboxProfile::Containerized` with namespace isolation
   - **Priority**: High
   - **Effort**: 16 hours

2. **No Secret Sanitization in Events**
   - **Risk**: EventBus publishes tool inputs/outputs that may contain secrets
   - **Impact**: Credential leakage to observability systems
   - **Mitigation**: Add `SecretSanitizer` subscriber to redact patterns
   - **Priority**: High
   - **Effort**: 8 hours

3. **Windows Shell Quoting Weakness**
   - **Risk**: cmd.exe quoting rules weaker than POSIX
   - **Impact**: Command injection on Windows
   - **Mitigation**: Prefer `bypass_shell: true` on Windows, document limitations
   - **Priority**: Medium
   - **Effort**: 4 hours

### High-Priority Gaps

4. **No Rate Limiting for API Server**
   - **Risk**: Denial of service, brute force attacks
   - **Impact**: Service unavailability
   - **Mitigation**: Add per-client rate limiting (e.g., 100 req/min)
   - **Priority**: Medium
   - **Effort**: 4 hours

5. **No Encryption for Audit Logs**
   - **Risk**: Sensitive metadata exposed if database file accessed
   - **Impact**: Privacy violation, reconnaissance
   - **Mitigation**: Encrypt SQLite database at rest (SQLCipher)
   - **Priority**: Medium
   - **Effort**: 8 hours

6. **No CPU Rate Limiting for Shell Commands**
   - **Risk**: Resource exhaustion
   - **Impact**: System slowdown, denial of service
   - **Mitigation**: Add cgroup/ulimit integration for shell processes
   - **Priority**: Low
   - **Effort**: 12 hours

### Medium-Priority Gaps

7. **No Network Egress Filtering for Plugins**
   - **Risk**: Plugins can make arbitrary HTTP requests
   - **Impact**: Data exfiltration, C2 communication
   - **Mitigation**: Add network capability with allowlist
   - **Priority**: Low
   - **Effort**: 8 hours

8. **No Integrity Verification for Plugin Binaries**
   - **Risk**: Tampered plugin files on disk
   - **Impact**: Code execution, sandbox escape
   - **Mitigation**: Add checksum verification on load
   - **Priority**: Low
   - **Effort**: 4 hours

9. **No Memory Encryption for Sensitive Data**
   - **Risk**: Memory dumps contain secrets
   - **Impact**: Credential theft
   - **Mitigation**: Use secure memory allocation (mlock, madvise)
   - **Priority**: Low
   - **Effort**: 8 hours

## Secure Development Guidelines

### Code Review Checklist

When reviewing code that touches security boundaries:

- [ ] **Input Validation**: All user/model inputs validated before use
- [ ] **Path Resolution**: File paths resolved under project root
- [ ] **Command Construction**: Shell commands use structured parsing or proper quoting
- [ ] **Secret Handling**: Secrets loaded from keychain, not hardcoded or logged
- [ ] **Error Messages**: Error messages do not include secrets or sensitive paths
- [ ] **Resource Limits**: All loops, allocations, and external calls bounded
- [ ] **Cancellation**: Long operations accept and respect `CancellationToken`
- [ ] **Audit Trail**: Security-relevant operations logged to audit trail
- [ ] **Capability Checks**: Plugin operations check manifest capabilities
- [ ] **Policy Evaluation**: Tool executions pass through policy engine

### Dependency Management

- **Minimal Dependencies**: Only add dependencies with clear justification
- **Regular Audits**: Run `cargo audit` and `cargo deny check` in CI
- **MSRV Policy**: Maintain minimum supported Rust version (currently 1.88)
- **Unmaintained Deps**: Pin or replace unmaintained transitive dependencies
- **Security Advisories**: Review and document all `RUSTSEC-*` exceptions in `deny.toml`

### Testing Requirements

- **Unit Tests**: All security-critical functions have unit tests
- **Integration Tests**: Policy engine, shell validation, filesystem boundary tested
- **Property Tests**: Shell parser, policy conditions, path resolution use proptest
- **Fuzz Tests**: Shell parser, plugin ABI parsing use cargo-fuzz
- **Security Tests**: Test cases for known attack patterns (injection, traversal, etc.)

### Incident Response

See [Incident Response](#incident-response) section below.

## Incident Response

### Detection

**Indicators of Compromise**:
- Unexpected tool executions in audit logs
- Unusual spend patterns (high cost, rapid requests)
- Plugin crashes or resource exhaustion
- Network connections to unexpected endpoints
- File system changes outside project root
- Shell commands matching denylist patterns

### Containment

**Immediate Actions**:
1. Cancel running agent loop (`CancellationToken`)
2. Disable affected plugin (remove from config)
3. Revoke API keys if exposed (rotate credentials)
4. Isolate affected project (move to quarantine directory)
5. Capture forensic snapshot (audit logs, memory state, file system)

### Investigation

**Steps**:
1. Review audit logs for attack timeline
2. Analyze plugin binaries (if plugin-related)
3. Check for persistence mechanisms (cron jobs, startup scripts)
4. Verify integrity of configuration files
5. Scan for malware (antivirus, rootkit detection)

### Remediation

**Actions**:
1. Rotate all credentials (API keys, SSH keys, tokens)
2. Update Concerto to latest version with security fixes
3. Reinstall affected plugins from trusted sources
4. Restore files from backup (if tampered)
5. Review and tighten policy rules

### Disclosure

**Process**:
1. Assess severity and impact
2. Prepare security advisory (CVE if applicable)
3. Notify affected users (if public release)
4. Publish fix and advisory
5. Post-mortem and lessons learned

**Contact**: Use private form at <https://mit-s.co.za/contact> with subject "Concerto security report"

## References

- [SECURITY_BOUNDARIES.md](../SECURITY_BOUNDARIES.md) - Detailed security boundaries
- [SECURITY.md](../SECURITY.md) - Security policy and reporting
- [ADR-001: Policy Engine Design](adrs/ADR-01.md) - Policy engine architecture
- [ADR-006: Plugin System](adrs/ADR-06.md) - Plugin sandbox design
- [OWASP Threat Modeling](https://owasp.org/www-community/Threat_Modeling) - Methodology reference
- [STRIDE](https://en.wikipedia.org/wiki/STRIDE_(security)) - Threat categorization

## Document History

| Date | Version | Author | Changes |
|------|---------|--------|---------|
| 2026-07-28 | 1.0 | Concerto Team | Initial threat model |

## Review Schedule

This threat model should be reviewed:
- **Quarterly**: Regular review of gaps and mitigations
- **After Incidents**: Update based on lessons learned
- **After Major Releases**: Reflect architectural changes
- **After New Features**: Assess new attack vectors

Next scheduled review: **2026-10-28**
