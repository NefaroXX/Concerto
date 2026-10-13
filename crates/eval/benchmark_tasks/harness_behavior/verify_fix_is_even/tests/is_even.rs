use verify_fix_is_even::is_even;

#[test]
fn is_even_identifies_even_numbers() {
    assert!(is_even(0));
    assert!(is_even(2));
    assert!(is_even(-4));
    assert!(!is_even(1));
    assert!(!is_even(7));
}
