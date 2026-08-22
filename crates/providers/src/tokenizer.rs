#[derive(Debug, Clone)]
pub struct TokenCount {
    pub count: u64,
    pub approximate: bool,
}

/// Count tokens for a text string.
pub fn count_tokens(text: &str, model: &str) -> TokenCount {
    if is_openai_model(model) {
        if let Ok(bpe) = tiktoken_rs::get_bpe_from_model(model) {
            let tokens = bpe.encode_with_special_tokens(text);
            return TokenCount { count: tokens.len() as u64, approximate: false };
        }
    }
    TokenCount { count: (text.len() as u64).div_ceil(4), approximate: true }
}

/// Count tokens for a message, including overhead for role markers.
pub fn count_message_tokens(content: &str, _role: &str, model: &str) -> TokenCount {
    let base = count_tokens(content, model);
    TokenCount { count: base.count + 4, approximate: base.approximate }
}

fn is_openai_model(model: &str) -> bool {
    model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("text-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_tokens_approximate() {
        let result = count_tokens("Hello, world!", "claude-3-5-sonnet");
        assert!(result.approximate);
        assert!(result.count > 0);
    }

    #[test]
    fn test_is_openai_model() {
        assert!(is_openai_model("gpt-4o"));
        assert!(is_openai_model("o1-preview"));
        assert!(!is_openai_model("claude-3-5-sonnet"));
    }
}
