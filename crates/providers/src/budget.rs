use concerto_core::types::TokenBudget;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Known model context capacities in tokens.
static MODEL_CAPACITIES: LazyLock<HashMap<&'static str, u64>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("gpt-4o", 128_000);
    m.insert("gpt-4o-mini", 128_000);
    m.insert("gpt-4-turbo", 128_000);
    m.insert("gpt-4", 8_192);
    m.insert("gpt-3.5-turbo", 16_385);
    m.insert("claude-3-5-sonnet", 200_000);
    m.insert("claude-3-5-haiku", 200_000);
    m.insert("claude-3-haiku", 200_000);
    m.insert("claude-3-opus", 200_000);
    m.insert("claude-2", 100_000);
    m.insert("gemini-1.5-pro", 1_000_000);
    m.insert("gemini-1.5-flash", 1_000_000);
    m.insert("gemini-2.0-flash", 1_000_000);
    // NVIDIA NIM — integrate.api.nvidia.com model IDs
    m.insert("meta/llama-3.1-8b-instruct", 128_000);
    m.insert("meta/llama-3.1-70b-instruct", 128_000);
    m.insert("meta/llama-3.1-405b-instruct", 128_000);
    m.insert("meta/llama-3.3-70b-instruct", 128_000);
    m.insert("nvidia/llama-3.1-nemotron-70b-instruct", 128_000);
    m.insert("mistralai/mixtral-8x7b-instruct-v0.1", 32_000);
    m.insert("mistralai/mistral-7b-instruct-v0.3", 32_000);
    // OpenRouter — common model IDs in openrouter.ai format
    m.insert("moonshotai/kimi-k2", 131_072);
    m.insert("anthropic/claude-3.5-sonnet", 200_000);
    m.insert("openai/gpt-4o", 128_000);
    m.insert("openai/gpt-4o-mini", 128_000);
    m.insert("meta-llama/llama-3.3-70b-instruct", 128_000);
    m.insert("deepseek/deepseek-chat", 64_000);
    // DeepSeek — api.deepseek.com native model IDs. The sibling
    // `deepseek/deepseek-chat` entry above is an OpenRouter-format ID (64K)
    // and stays untouched; these native IDs are matched exactly and never
    // collide with it.
    m.insert("deepseek-chat", 1_000_000);
    m.insert("deepseek-reasoner", 1_000_000);
    // Groq — api.groq.com model IDs
    m.insert("llama-3.3-70b-versatile", 131_072);
    m.insert("openai/gpt-oss-120b", 131_072);
    m.insert("openai/gpt-oss-20b", 131_072);
    // Together AI — api.together.xyz model IDs
    m.insert("meta-llama/Llama-3.3-70B-Instruct-Turbo", 131_072);
    m.insert("meta-llama/Llama-4-Scout-17B-16E-Instruct", 1_000_000);
    m.insert("deepseek-ai/DeepSeek-V3", 128_000);
    // Mistral — api.mistral.ai model IDs
    m.insert("mistral-large-latest", 131_072);
    m.insert("mistral-medium-latest", 131_072);
    m.insert("mistral-small-latest", 131_072);
    m.insert("codestral-latest", 256_000);
    // xAI — api.x.ai model IDs
    m.insert("grok-4", 256_000);
    m.insert("grok-4-fast", 2_000_000);
    m.insert("grok-2-latest", 131_072);
    // Fireworks — api.fireworks.ai/inference model IDs
    m.insert("accounts/fireworks/models/llama-v3p3-70b-instruct", 128_000);
    m.insert("accounts/fireworks/models/llama-4-maverick", 1_000_000);
    m.insert("accounts/fireworks/models/deepseek-v3", 128_000);
    // Cerebras — api.cerebras.ai model IDs
    m.insert("llama-3.3-70b", 128_000);
    m.insert("gpt-oss-120b", 131_072);
    m.insert("llama3.1-8b", 131_072);
    m.insert("qwen-3-32b", 131_072);
    // Cohere — api.cohere.com/compatibility model IDs
    m.insert("command-a", 256_000);
    m.insert("command-a-plus-05-2026", 128_000);
    m.insert("command-r-plus-08-2024", 128_000);
    m.insert("command-r-08-2024", 128_000);
    m
});

const DEFAULT_CAPACITY: u64 = 128_000;

/// Look up the context capacity for a given model string.
pub fn capacity_for_model(model: &str) -> u64 {
    if let Some(&cap) = MODEL_CAPACITIES.get(model) {
        return cap;
    }
    let mut best_match: Option<(&str, u64)> = None;
    for (key, &cap) in MODEL_CAPACITIES.iter() {
        if model.starts_with(key)
            && best_match.is_none_or(|(best_key, _)| key.len() > best_key.len())
        {
            best_match = Some((key, cap));
        }
    }
    best_match.map_or(DEFAULT_CAPACITY, |(_, cap)| cap)
}

/// Create a TokenBudget for a given model.
pub fn budget_for_model(model: &str, reserved_for_response: u64) -> TokenBudget {
    let capacity = capacity_for_model(model);
    TokenBudget::new(capacity, reserved_for_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_model_capacity() {
        assert_eq!(capacity_for_model("gpt-4o"), 128_000);
        assert_eq!(capacity_for_model("claude-3-5-sonnet"), 200_000);
    }

    #[test]
    fn test_unknown_model_falls_back() {
        assert_eq!(capacity_for_model("some-future-model-v2"), DEFAULT_CAPACITY);
    }

    #[test]
    fn test_prefix_match() {
        assert_eq!(capacity_for_model("gpt-4o-2024-08-06"), 128_000);
    }

    /// DeepSeek direct API IDs carry the full 1M context; the OpenRouter-
    /// format `deepseek/deepseek-chat` ID keeps its own (smaller) capacity
    /// even though it shares the `deepseek` stem.
    #[test]
    fn test_deepseek_native_capacities() {
        assert_eq!(capacity_for_model("deepseek-chat"), 1_000_000);
        assert_eq!(capacity_for_model("deepseek-reasoner"), 1_000_000);
        assert_eq!(capacity_for_model("deepseek/deepseek-chat"), 64_000);
    }

    /// The integrated Tier-1 OpenAI-compatible providers carry native model
    /// capacities for the models they are configured with by default (the
    /// provider-level tests assert the exact defaults; these are the budget
    /// table's own contract).
    #[test]
    fn test_tier1_native_capacities() {
        assert_eq!(capacity_for_model("llama-3.3-70b-versatile"), 131_072);
        assert_eq!(capacity_for_model("meta-llama/Llama-3.3-70B-Instruct-Turbo"), 131_072);
        assert_eq!(capacity_for_model("mistral-large-latest"), 131_072);
        assert_eq!(capacity_for_model("grok-4"), 256_000);
        assert_eq!(
            capacity_for_model("accounts/fireworks/models/llama-v3p3-70b-instruct"),
            128_000
        );
        assert_eq!(capacity_for_model("llama-3.3-70b"), 128_000);
        assert_eq!(capacity_for_model("command-a-plus-05-2026"), 128_000);
    }

    #[test]
    fn test_budget_for_model() {
        let budget = budget_for_model("gpt-4o", 4_000);
        assert_eq!(budget.capacity, 128_000);
        assert_eq!(budget.reserved_for_response, 4_000);
        assert_eq!(budget.available, 124_000);
    }
}
