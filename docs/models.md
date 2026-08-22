# Provider and model configuration

Concerto keeps a list of provider configurations and resolves a provider/model
pair for the current session and, optionally, for each multi-agent role. The
Settings page is the preferred way to configure these values because it also
tests credentials and discovers models where a provider supports discovery.

## Supported provider IDs

| ID | Provider |
|---|---|
| `openai` | OpenAI and explicitly configured compatible endpoints |
| `anthropic` | Anthropic |
| `google` | Google Gemini |
| `openrouter` | OpenRouter |
| `nim` | NVIDIA NIM |
| `ollama` | Local or remote Ollama |
| `opencode` | OpenCode-compatible provider |

Provider and model identifiers are case-sensitive API values. Friendly names
are display labels only.

## Configuration locations

The global file is the platform Concerto configuration file (normally
`~/.config/concerto/config.toml` on Linux). A project-root `.concerto.toml` can
override project-specific settings. `CONCERTO_*` environment configuration is
also supported; legacy `opencode-rs` paths and `OPENCODE_RS_*` variables remain
recognized for migration. Do not set both prefixes for the same value.

## Provider list and default

```toml
[model_settings]
global_default_id = "claude-main"
# global_default_model = "a-model-override"

[[model_settings.providers]]
id = "claude-main"
name = "Claude main"
provider = "anthropic"
model = "your-anthropic-model-id"
timeout_seconds = 60
keyring_key = "anthropic/api_key"

[[model_settings.providers]]
id = "local-ollama"
name = "Local Ollama"
provider = "ollama"
model = "your-local-model-id"
api_base = "http://localhost:11434"
timeout_seconds = 120
keyring_key = "ollama/api_key"
```

Each `id` must be stable and unique; agent assignments reference it. If
`global_default_id` is absent, Concerto uses the first configured provider.
`global_default_model` overrides that provider's default model for the global
selection.

Model discovery updates `cached_models` metadata. It does not silently replace
the model the user selected.

### Extra models per provider

A provider config can advertise additional model names that it serves without
adding a separate `[[model_settings.providers]]` block — useful when one
`api_base` points at an OpenAI-compatible gateway that exposes several models:

```toml
[[model_settings.providers]]
id = "gateway"
name = "Compat gateway"
provider = "openai"
model = "primary-model"
api_base = "https://gateway.example.com/v1"
timeout_seconds = 60
keyring_key = "gateway/api_key"
extra_models = ["alias-model-a", "alias-model-b"]
```

`extra_models` participates in model *resolution* only: `config_for_model` and
the model picker treat each entry as a model the provider offers. It never
shadows the primary `model` (the primary always wins on exact match), and it
does not create extra routing profiles (profiles stay one per provider).

### Reasoning-content echo

For OpenAI-compatible providers (`openai`, `openrouter`, `nim`, `opencode`),
`reasoning_echo` controls whether captured `reasoning_content` is echoed back
on assistant messages (ADR-46). DeepSeek-style endpoints reject tool-call
histories whose assistant messages carry stale reasoning, so `"always"` emits
`reasoning_content` (empty when none) on every assistant message.

```toml
[[model_settings.providers]]
id = "deepseek-gateway"
name = "DeepSeek via gateway"
provider = "openai"
model = "deepseek-r1"
reasoning_echo = "always"
```

Valid values: `"always"`, `"if-present"` (emit only when captured reasoning
exists; the provider default). Omit the field to leave the provider's built-in
behavior untouched. Unsupported values log a warning and are treated as unset —
they never fail to load.

## Agent assignments

Valid roles are `coordinator`, `architect`, `researcher`, `coder`, `reviewer`,
and `validator`.

```toml
[[model_settings.agent_assignments]]
agent_role = "coordinator"
provider_config_id = "claude-main"

[[model_settings.agent_assignments]]
agent_role = "coder"
provider_config_id = "local-ollama"
model_override = "a-tool-capable-local-model"
```

An assignment selects both a provider configuration and a model. Enabling
multi-agent mode does not intentionally replace an explicit assignment. A role
without an assignment inherits the session selection or uses compatible
fallback routing when no explicit/session pair exists.

## Selection rules

Concerto no longer uses subjective capability tiers.

1. Resolve an explicit role assignment, otherwise the session provider/model.
2. Validate that the referenced provider exists and the pair can be built.
3. Enforce objective requirements. Researcher, Coder, and Validator require
   tool-call support; other roles do not automatically require it.
4. Check the remaining spend budget.
5. Only where no authoritative pair exists, select the lowest-cost compatible
   profile.
6. Emit the resolved provider/model in routing events and visible run state.

Explicit assignments are not silently “upgraded” to a different subjective
tier. If metadata incorrectly says a model cannot call tools, correct that
metadata or select a compatible model.

## Profile metadata overrides

Overrides are keyed by provider configuration ID:

```toml
[model_settings.model_profile_overrides]
"claude-main" = {
  context_window = 200000,
  supports_tool_calling = true,
  cost_per_1k_tokens = 0.01,
  avg_latency_ms = 1500,
  description = "Project-specific metadata"
}
```

Available fields are `cost_per_1k_tokens`, `avg_latency_ms`,
`context_window`, `supports_tool_calling`, `base_url`, and `description`.
Prices and model features change; treat built-in metadata as a routing aid, not
an authoritative provider catalogue.

## Credentials

Use the setup wizard or Settings page to store API keys in the operating-system
credential store. The TOML file stores only `keyring_key`, never the secret.

Credential lookup in test mode is environment-backed: `anthropic/api_key`
becomes `CONCERTO_ANTHROPIC_API_KEY`. Test mode is selected per call by
`CredentialStore::from_env()`; tests and CI construct it directly. The
`CONCERTO_TEST_MODE=1` env var is documented for parity but is not read by the
code. Env-backed lookup is for automated tests; do not enable it as a general
replacement for the OS credential store.

## Troubleshooting

| Symptom | Check |
|---|---|
| Single-agent works but a specialist gets auth errors | Verify that specialist's `provider_config_id`, credential entry, API base, and model override—not only the chat selection |
| Coder cannot be dispatched | Confirm the resolved model is marked as supporting tool calls |
| Unexpected provider/model | Check the role assignment, session selection, global default, then fallback routing in that order |
| Model missing from picker | Refresh discovery or use the custom model-ID entry; confirm the provider API exposes it |
| Rate-limit loop | Check provider limits and `[retry]`; retries preserve a run but cannot create quota |
| Cost rejection | Check the session cap, multi-agent multiplier, and model cost override |

There is no `concerto providers list` command. Use Settings/Quick Panel in the
desktop application or inspect the configuration file.
