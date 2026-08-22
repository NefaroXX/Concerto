use reqwest;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_todo_crud() {
    let mut child = Command::new("cargo")
        .args(["run"])
        .current_dir("..")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start server");

    sleep(Duration::from_secs(2)).await;

    let client = reqwest::Client::new();

    let create_res = client
        .post("http://127.0.0.1:3000/todos")
        .json(&serde_json::json!({"text": "Buy milk"}))
        .send()
        .await;
    let _ = child.kill();

    let res = create_res.expect("failed to create todo");
    assert!(res.status().is_success());
}
