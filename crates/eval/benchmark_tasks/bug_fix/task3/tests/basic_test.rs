use fix_race_counter::{run_concurrent_increments};

#[test]
fn test_single_threaded_increment() {
    let result = run_concurrent_increments(1, 100);
    assert_eq!(result, 100);
}

#[test]
fn test_multi_threaded_increment() {
    let result = run_concurrent_increments(10, 100);
    assert_eq!(result, 1000);
}

#[test]
fn test_large_concurrent_increments() {
    let result = run_concurrent_increments(100, 100);
    assert_eq!(result, 10000);
}
