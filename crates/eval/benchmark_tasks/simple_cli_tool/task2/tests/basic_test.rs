use std::fs;
use std::process::Command;

#[test]
fn test_line_counter_file() {
    let test_file = "test_input.txt";
    fs::write(test_file, "hello world\nfoo bar\n").unwrap();
    let output = Command::new("cargo")
        .args(["run", "--", test_file])
        .current_dir("..")
        .output()
        .expect("failed to execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Lines: 2"));
    assert!(stdout.contains("Words: 4"));
    let _ = fs::remove_file(test_file);
}
