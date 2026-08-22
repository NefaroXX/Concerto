pub fn binary_search(arr: &[i32], target: i32) -> Option<usize> {
    let mut left = 0usize;
    let mut right = arr.len() as i32 - 1;
    while left <= right as usize {
        let mid = left + (right as usize - left) / 2;
        if arr[mid] == target {
            return Some(mid);
        } else if arr[mid] < target {
            left = mid + 2; // off-by-one: skips one element
        } else {
            right = mid as i32 - 1;
        }
    }
    None
}
