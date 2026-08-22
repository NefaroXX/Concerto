use std::sync::Arc;
use std::thread;

// NOTE: This implementation has a deliberate race condition.
// The increment is not atomic, so concurrent threads may lose updates.
pub struct Counter {
    value: *mut i32,
}

unsafe impl Send for Counter {}
unsafe impl Sync for Counter {}

impl Counter {
    pub fn new() -> Self {
        let boxed = Box::new(0);
        Self { value: Box::into_raw(boxed) }
    }

    pub fn increment(&self) {
        unsafe {
            *self.value += 1; // Race: read-modify-write is not atomic
        }
    }

    pub fn get(&self) -> i32 {
        unsafe { *self.value }
    }
}

pub fn run_concurrent_increments(num_threads: usize, increments_per_thread: usize) -> i32 {
    let counter = Arc::new(Counter::new());
    let mut handles = vec![];

    for _ in 0..num_threads {
        let counter_clone = Arc::clone(&counter);
        let handle = thread::spawn(move || {
            for _ in 0..increments_per_thread {
                counter_clone.increment();
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    counter.get()
}
