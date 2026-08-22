use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct TokenBucket {
    tokens: Mutex<f64>,
    capacity: f64,
    last_update: Mutex<Instant>,
    refill_rate: f64,
}

impl TokenBucket {
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            tokens: Mutex::new(capacity),
            capacity,
            last_update: Mutex::new(Instant::now()),
            refill_rate,
        }
    }

    pub fn allow(&self) -> bool {
        let mut tokens = self.tokens.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(*self.last_update.lock().unwrap()).as_secs_f64();
        *tokens = (self.capacity).min(*tokens + elapsed * self.refill_rate);
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }

    pub fn check(&self) -> bool {
        let _tokens = self.tokens.lock().unwrap();
        _tokens.clone() >= 1.0
    }
}
