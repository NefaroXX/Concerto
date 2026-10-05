pub fn allowed(value: u8) -> bool {
    value == 7
}

#[cfg(test)]
mod tests {
    use super::allowed;

    // verifies: both allowed and denied inputs reject constant and operator mutations.
    #[test]
    fn checks_every_input() {
        for value in 0..=u8::MAX {
            assert_eq!(allowed(value), value == 7);
        }
    }
}
