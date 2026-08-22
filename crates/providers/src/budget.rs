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

    #[test]
    fn test_budget_for_model() {
        let budget = budget_for_model("gpt-4o", 4_000);
        assert_eq!(budget.capacity, 128_000);
        assert_eq!(budget.reserved_for_response, 4_000);
        assert_eq!(budget.available, 124_000);
    }
}
