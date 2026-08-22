use benchmark_task_6::*;

#[test]
fn test_parse_number_valid() {
    assert_eq!(parse_number("42"), Ok(42));
    assert_eq!(parse_number("-3"), Ok(-3));
}

#[test]
fn test_parse_number_invalid() {
    assert!(parse_number("abc").is_err());
    assert!(parse_number("").is_err());
}
