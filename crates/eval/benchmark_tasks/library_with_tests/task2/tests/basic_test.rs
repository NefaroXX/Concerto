use rate_limiter_lib::TokenBucket;
use std::sync::Arc;
use std::thread;

#[test]
fn test_token_bucket_basic() {
    let bucket = TokenBucket::new(10.0, 1.0);
    assert!(bucket.allow());
    assert!(bucket.allow());
}

#[test]
fn test_token_bucket_exhaustion() {
    let bucket = TokenBucket::new(1.0, 0.0);
    assert!(bucket.allow());
    assert!(!bucket.allow());
}

#[test]
fn test_concurrent_access() {
    let bucket = Arc::new(TokenBucket::new(100.0, 0.0));
    let mut handles = vec![];

    for _ in 0..10 {
        let b = Arc::clone(&bucket);
        let handle = thread::spawn(move || {
            for _ in 0..10 {
                b.allow();
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}
