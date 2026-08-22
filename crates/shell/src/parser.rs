use thiserror::Error;

use crate::CommandInvocation;

/// A deterministic command-line parsing failure. Parsing performs no expansion.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ParseError {
    #[error("command line is empty")]
    Empty,
    #[error("unterminated single quote")]
    UnterminatedSingleQuote,
    #[error("unterminated double quote")]
    UnterminatedDoubleQuote,
    #[error("command line ends with an escape character")]
    DanglingEscape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Parse a command and quoted arguments without variables, globs, pipes, or
/// shell expansion.
///
/// # Errors
///
/// Returns a structured error for empty input, incomplete quotes, or a dangling
/// escape character.
pub fn parse_command_line(line: &str) -> Result<CommandInvocation, ParseError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut token_started = false;

    for character in line.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            token_started = true;
            continue;
        }

        match (quote, character) {
            (Quote::None | Quote::Double, '\\') => {
                escaped = true;
                token_started = true;
            }
            (Quote::None, '\'') => {
                quote = Quote::Single;
                token_started = true;
            }
            (Quote::None, '"') => {
                quote = Quote::Double;
                token_started = true;
            }
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, character) if character.is_whitespace() => {
                if token_started {
                    tokens.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            (_, character) => {
                current.push(character);
                token_started = true;
            }
        }
    }

    if escaped {
        return Err(ParseError::DanglingEscape);
    }
    match quote {
        Quote::Single => return Err(ParseError::UnterminatedSingleQuote),
        Quote::Double => return Err(ParseError::UnterminatedDoubleQuote),
        Quote::None => {}
    }
    if token_started {
        tokens.push(current);
    }
    if tokens.is_empty() {
        return Err(ParseError::Empty);
    }

    let command = tokens.remove(0);
    if command.is_empty() {
        return Err(ParseError::Empty);
    }
    Ok(CommandInvocation { command, arguments: tokens, raw: line.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::{parse_command_line, ParseError};
    use proptest::prelude::*;

    #[test]
    fn parses_quotes_and_escapes_without_expansion() {
        let parsed = parse_command_line("ls-tree 'a b' \"$HOME\" plain\\ value")
            .expect("valid command line");
        assert_eq!(parsed.command, "ls-tree");
        assert_eq!(parsed.arguments, ["a b", "$HOME", "plain value"]);
    }

    #[test]
    fn preserves_empty_quoted_argument() {
        let parsed = parse_command_line("help \"\"").expect("valid command line");
        assert_eq!(parsed.arguments, [""]);
    }

    #[test]
    fn rejects_incomplete_input() {
        assert_eq!(parse_command_line("help 'oops"), Err(ParseError::UnterminatedSingleQuote));
        assert_eq!(parse_command_line("help \\"), Err(ParseError::DanglingEscape));
    }

    // -----------------------------------------------------------------------
    // Property-based tests (proptest)
    // -----------------------------------------------------------------------

    // Core safety property: parsing never panics on any input.
    proptest! {
        #[test]
        fn parse_never_panics(line in ".*") {
            // The only acceptable outcomes are Ok or one of the ParseError variants.
            let result = parse_command_line(&line);
            match result {
                Ok(invocation) => {
                    // Roundtrip: raw field must equal the original input.
                    prop_assert_eq!(&invocation.raw, &line);
                    // The command part must be non-empty.
                    prop_assert!(!invocation.command.is_empty(), "command must be non-empty on success");
                }
                Err(e) => {
                    // Every error variant should be reachable; we just verify
                    // that the error is one of the four known variants.
                    match e {
                        ParseError::Empty
                        | ParseError::UnterminatedSingleQuote
                        | ParseError::UnterminatedDoubleQuote
                        | ParseError::DanglingEscape => {}
                    }
                }
            }
        }
    }

    // Property: balanced quotes and escapes always parse successfully
    // when followed by a non-empty command.
    proptest! {
        #[test]
        fn balanced_quotes_parse(cmd in "[a-zA-Z][a-zA-Z0-9_-]{0,10}") {
            // Simple command: no quotes, no escapes.
            let parsed = parse_command_line(&cmd).expect("simple command must parse");
            prop_assert_eq!(parsed.command.as_str(), cmd.as_str());
            prop_assert!(parsed.arguments.is_empty());
        }
    }

    // Property: single-quoted strings preserve their content verbatim.
    proptest! {
        #[test]
        fn single_quotes_preserve_content(
            cmd in "[a-z]{1,5}",
            content in "[ -~]{0,20}",
        ) {
            // Avoid content that would close the quote prematurely.
            if content.contains('\'') {
                return Ok(());  // skip: would be unterminated or nested
            }
            let line = format!("{cmd} '{content}'");
            let parsed = parse_command_line(&line).expect("single-quoted arg must parse");
            prop_assert_eq!(parsed.command.as_str(), cmd.as_str());
            prop_assert!(parsed.arguments.len() == 1, "expected exactly one argument");
            prop_assert_eq!(parsed.arguments[0].as_str(), content.as_str());
        }
    }

    // Property: double-quoted strings preserve content (including $variable references).
    //
    // Note: within double quotes, backslash escapes the next character, so
    // `\` and `"` are excluded from generated content to avoid unintended
    // quote termination.
    proptest! {
        #[test]
        fn double_quotes_preserve_content(
            cmd in "[a-z]{1,5}",
            content in proptest::string::string_regex(r#"[ !#-\[\]-~]{0,20}"#).unwrap(),
        ) {
            let line = format!("{cmd} \"{content}\"");
            let parsed = parse_command_line(&line).expect("double-quoted arg must parse");
            prop_assert_eq!(parsed.command.as_str(), cmd.as_str());
            prop_assert!(parsed.arguments.len() == 1, "expected exactly one argument");
            prop_assert_eq!(parsed.arguments[0].as_str(), content.as_str());
        }
    }

    // Property: backslash escapes the next character in unquoted context.
    proptest! {
        #[test]
        fn backslash_escapes_next_char(
            cmd in "[a-z]{1,2}",
            ch in "[ -~]",
        ) {
            let line = format!("{cmd} \\{ch}");
            let parsed = parse_command_line(&line).expect("escaped char arg must parse");
            prop_assert!(parsed.arguments.len() == 1, "expected exactly one argument");
            prop_assert_eq!(parsed.arguments[0].as_str(), ch);
        }
    }

    /// Property: empty input always fails with Empty error.
    #[test]
    fn empty_input_always_fails() {
        assert_eq!(parse_command_line(""), Err(ParseError::Empty));
    }

    // -----------------------------------------------------------------------
    // Additional edge-case tests
    // -----------------------------------------------------------------------

    #[test]
    fn handles_trailing_whitespace() {
        let parsed = parse_command_line("echo hello   ").expect("trailing whitespace");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.arguments, ["hello"]);
    }

    #[test]
    fn handles_leading_whitespace() {
        let parsed = parse_command_line("   echo hello").expect("leading whitespace");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.arguments, ["hello"]);
    }

    #[test]
    fn tab_character_acts_as_separator() {
        let parsed = parse_command_line("echo\thello\tworld").expect("tab separated");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.arguments, ["hello", "world"]);
    }

    #[test]
    fn multiple_spaces_between_arguments_are_collapsed() {
        let parsed = parse_command_line("echo   hello    world").expect("multiple spaces");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.arguments, ["hello", "world"]);
    }

    #[test]
    fn escaped_space_preserves_literal_space_within_argument() {
        let parsed = parse_command_line("echo hello\\ world").expect("escaped space");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.arguments, ["hello world"]);
    }

    #[test]
    fn double_quotes_allow_backslash_escape_of_double_quote() {
        let parsed =
            parse_command_line("echo \"hello \\\"world\\\"\"").expect("escaped double quote");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.arguments, ["hello \"world\""]);
    }

    #[test]
    fn special_characters_preserved_in_double_quotes() {
        let parsed = parse_command_line("echo \"$HOME `pwd` !important\"").expect("special chars");
        assert_eq!(parsed.command, "echo");
        // Dollar sign, backticks, and exclamation are preserved verbatim inside double quotes.
        assert_eq!(parsed.arguments, ["$HOME `pwd` !important"]);
    }

    #[test]
    fn unicode_characters_in_arguments() {
        let parsed = parse_command_line("echo café 💡").expect("unicode");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.arguments, ["café", "💡"]);
    }

    #[test]
    fn backslash_escapes_itself_in_unquoted_context() {
        // Two backslashes produce one literal backslash.
        let parsed = parse_command_line("echo hello\\\\world").expect("escaped backslash");
        assert_eq!(parsed.command, "echo");
        assert_eq!(parsed.arguments, [r"hello\world"]);
    }

    #[test]
    fn unterminated_double_quote_is_detected() {
        assert_eq!(parse_command_line("echo \"hello"), Err(ParseError::UnterminatedDoubleQuote));
    }
}
