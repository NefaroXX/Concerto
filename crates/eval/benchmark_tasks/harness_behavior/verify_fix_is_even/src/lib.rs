//! A seeded correctness bug: `is_even` returns the opposite of the truth.

pub fn is_even(n: i64) -> bool {
    n % 2 == 1
}
