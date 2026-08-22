use benchmark_task_2::*;

#[test]
fn test_multiply() {
    assert_eq!(multiply(3, 4), 12);
    assert_eq!(multiply(0, 5), 0);
    assert_eq!(multiply(-2, 3), -6);
}
