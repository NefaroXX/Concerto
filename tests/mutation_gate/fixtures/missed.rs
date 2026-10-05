pub fn allowed(value: u8) -> bool {
    value == 7
}

#[cfg(test)]
mod tests {
    use super::allowed;

    // verifies: deliberately weak coverage lets a constant-true mutant survive.
    #[test]
    fn only_checks_allowed_input() {
        assert!(allowed(7));
    }
}
