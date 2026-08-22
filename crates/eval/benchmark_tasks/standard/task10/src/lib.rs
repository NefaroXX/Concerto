pub struct PasswordValidator;

impl PasswordValidator {
    pub fn new() -> Self {
        PasswordValidator
    }

    pub fn validate(&self, password: &str) -> bool {
        password.len() >= 8
    }
}
