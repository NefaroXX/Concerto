use retry_signature_change::parse_port;

#[test]
fn parse_port_reports_bad_input_instead_of_panicking() {
    assert_eq!(parse_port("8080"), Ok(8080));
    assert!(parse_port("nope").is_err());
}
