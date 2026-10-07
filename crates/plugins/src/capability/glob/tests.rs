use super::*;
// --- glob_match ---

#[test]
fn glob_exact_match() {
    assert!(glob_match("src/main.rs", std::path::Path::new("src/main.rs")));
}

#[test]
fn glob_wildcard_match() {
    assert!(glob_match("*.rs", std::path::Path::new("main.rs")));
}

#[test]
fn glob_wildcard_no_match() {
    assert!(!glob_match("*.rs", std::path::Path::new("main.js")));
}

#[test]
fn glob_double_star_match() {
    assert!(glob_match("src/**/*.rs", std::path::Path::new("src/a/b/c.rs")));
}

#[test]
fn glob_question_mark() {
    assert!(glob_match("?.rs", std::path::Path::new("a.rs")));
    assert!(!glob_match("?.rs", std::path::Path::new("ab.rs")));
}
