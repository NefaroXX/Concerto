use reqwest;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_healthz_endpoint() {
    let mut child = Command::new("cargo")
        .args(["run"])
        .current_dir("..")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start server");

    sleep(Duration::from_secs(2)).await;

    let client = reqwest::Client::new();
    let res = client.get("http://127.0.0.1:3000/healthz").send().await;
    let _ = child.kill();

    let res = res.expect("failed to send request");
    assert!(res.status().is_success());
    let body = res.text().await.expect("failed to read body");
    assert!(body.contains("ok"));
}
