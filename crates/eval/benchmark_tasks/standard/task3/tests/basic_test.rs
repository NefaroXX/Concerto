use benchmark_task_3::*;

#[test]
fn test_is_even() {
    assert!(is_even(2));
    assert!(is_even(0));
    assert!(is_even(-4));
    assert!(!is_even(1));
    assert!(!is_even(-3));
}
