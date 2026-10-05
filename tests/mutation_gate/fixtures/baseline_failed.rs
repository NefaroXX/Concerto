pub fn allowed(value: u8) -> bool {
    value == 7
}

#[cfg(test)]
mod tests {
    use super::allowed;

    // verifies: an intentionally failing unmutated test is a baseline failure.
    #[test]
    fn intentionally_wrong_baseline() {
        assert!(!allowed(7));
    }
}
