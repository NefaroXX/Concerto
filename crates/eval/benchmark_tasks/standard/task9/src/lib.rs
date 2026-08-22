pub fn sum_range(start: u32, end: u32) -> u64 {
    let mut sum = 0u64;
    for i in (start as u64)..=(end as u64) {
        sum += i;
    }
    sum
}
