use benchmark_task_8::*;

#[test]
fn test_greet_with_greeting() {
    assert_eq!(greet("Alice", "Hi"), "Hi, Alice!");
    assert_eq!(greet("Bob", "Hey"), "Hey, Bob!");
}

#[test]
fn test_greet_default() {
    assert_eq!(greet_default("Charlie"), "Hello, Charlie!");
}
