use benchmark_task_10::*;

#[test]
fn test_password_with_digit() {
    let validator = PasswordValidator::new();
    assert!(validator.validate("Password1"));
    assert!(validator.validate("hello2world"));
}

#[test]
fn test_password_without_digit() {
    let validator = PasswordValidator::new();
    assert!(!validator.validate("Password"));
    assert!(!validator.validate("helloworld"));
}

#[test]
fn test_password_too_short() {
    let validator = PasswordValidator::new();
    assert!(!validator.validate("Pass1"));
    assert!(!validator.validate("12345"));
}
