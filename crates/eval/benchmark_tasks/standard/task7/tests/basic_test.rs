use benchmark_task_7::*;

#[test]
fn test_fibonacci_base_cases() {
    assert_eq!(fibonacci(0), 0);
    assert_eq!(fibonacci(1), 1);
}

#[test]
fn test_fibonacci_small_values() {
    assert_eq!(fibonacci(2), 1);
    assert_eq!(fibonacci(3), 2);
    assert_eq!(fibonacci(4), 3);
    assert_eq!(fibonacci(5), 5);
    assert_eq!(fibonacci(10), 55);
}

#[test]
fn test_fibonacci_boundary() {
    assert_eq!(fibonacci(20), 6765);
}
