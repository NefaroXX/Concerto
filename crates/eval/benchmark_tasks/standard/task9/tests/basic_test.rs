use benchmark_task_9::*;

#[test]
fn test_sum_range() {
    assert_eq!(sum_range(1, 3), 6);
    assert_eq!(sum_range(5, 5), 5);
    assert_eq!(sum_range(0, 10), 55);
    assert_eq!(sum_range(100, 200), 15050);
}
