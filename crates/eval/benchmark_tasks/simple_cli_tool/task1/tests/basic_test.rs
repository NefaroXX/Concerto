use std::process::Command;

#[test]
fn test_echo_basic() {
    let output = Command::new("cargo")
        .args(["run", "--", "--text", "hello"])
        .current_dir("..")
        .output()
        .expect("failed to execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.trim().contains("hello"));
}

#[test]
fn test_echo_uppercase() {
    let output = Command::new("cargo")
        .args(["run", "--", "--text", "hello", "--uppercase"])
        .current_dir("..")
        .output()
        .expect("failed to execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.trim().contains("HELLO"));
}
