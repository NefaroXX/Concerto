//! Live end-to-end prompt-cache checks for the anonymous OpenCode Zen free
//! models.
//!
//! Each test drives the shipped headless CLI (`concerto run --json`) in an
//! isolated `HOME`/`XDG_*` fixture, then asserts the persisted session's cache
//! report through `concerto sessions cache <id> --json`.
//!
//! `#[ignore]` plus the `CONCERTO_LIVE_OPENCODE=1` runtime gate keeps the
//! default suite offline. Run with:
//!
//! ```text
//! CONCERTO_LIVE_OPENCODE=1 cargo test -p concerto-cli \
//!     --test opencode_cache_live -- --ignored --nocapture
//! ```
//!
//! Unlike the provider-level probe, a model that is rate limited here fails
//! the test with the captured stdout/stderr: this harness exists to observe a
//! real agent run, so a silent skip would hide a broken cache path.
//!
//! Assertions are per model: DeepSeek must clear 50% aggregate token reuse
//! because its cache is content-addressed; GLM must show at least two cached
//! calls and one call reusing >=30% of its prompt, because its anonymous free
//! tier serves from uncoordinated relay replicas (the provider-level probe
//! still owns GLM's strict >=50% single-continuation guarantee, with bounded
//! retries).

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const DEEPSEEK: &str = "deepseek-v4.1-flash-free";
const GLM: &str = "glm-5.3-flash-free";

/// Generous process-level ceiling: `--timeout 300` bounds the agent loop, and
/// this leaves startup, teardown, and the persisted-session tail ample room.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(600);

fn live_enabled() -> bool {
    match std::env::var("CONCERTO_LIVE_OPENCODE") {
        Ok(value) if value.trim() == "1" => true,
        _ => {
            eprintln!(
                "SKIP live OpenCode CLI cache run: set CONCERTO_LIVE_OPENCODE=1 to hit the free tier"
            );
            false
        }
    }
}

#[test]
#[ignore = "live network: set CONCERTO_LIVE_OPENCODE=1, run with --ignored"]
fn deepseek_free_model_end_to_end_cache_hit() {
    if !live_enabled() {
        return;
    }
    run_model(DEEPSEEK);
}

#[test]
#[ignore = "live network: set CONCERTO_LIVE_OPENCODE=1, run with --ignored"]
fn glm_free_model_end_to_end_cache_hit() {
    if !live_enabled() {
        return;
    }
    run_model(GLM);
}

/// One isolated project + config, mirroring the CLI settings test fixtures.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(model: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project");

        fs::write(project.join("alpha.txt"), "ALPHA is the first word in this file.\n")
            .expect("write alpha");
        fs::write(project.join("beta.txt"), "BETA is the first word in this file.\n")
            .expect("write beta");
        fs::write(project.join("gamma.txt"), "GAMMA is the first word in this file.\n")
            .expect("write gamma");

        // ~29 KiB of project conventions: large enough to push the stable
        // system prefix above GLM's observed ~6k-token cache floor while
        // staying inside the 32 KiB default `project_context` budget.
        let mut agents = String::from("# Project conventions\n\n");
        for line in 0..300 {
            agents.push_str(&format!(
                "{line:03}: Keep changes minimal, prefer verified behavior over cleverness, and \
                 document invariants and failure recovery in the code you touch.\n"
            ));
        }
        fs::write(project.join("AGENTS.md"), agents).expect("write AGENTS.md");

        let config_dir = dir.path().join("config/concerto");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(
            config_dir.join("config.toml"),
            format!(
                r#"schema_version = 8

[model_settings]
global_default_model = "{model}"

[[model_settings.providers]]
id = "zen"
name = "Zen"
provider = "opencode"
model = "{model}"
timeout_seconds = 120
keyring_key = "zen/api_key"

[context]
cache_stable_prefix = true

[memory]
enabled = false

[project_context]
enabled = true
"#
            ),
        )
        .expect("write config");

        fs::create_dir_all(dir.path().join("home")).expect("create home");
        fs::create_dir_all(dir.path().join("data")).expect("create data");
        Self { dir }
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_concerto-cli"));
        command
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("HOME", self.dir.path().join("home"))
            .env_remove("CONCERTO_PROJECT_ROOTS")
            .args(["--project", self.project().to_str().expect("project path is UTF-8")])
            .args(args);
        command
    }

    /// Run a command to completion with the fixture environment and a hard
    /// wall-clock ceiling. Output is redirected to files so a chatty child
    /// cannot deadlock on a full pipe while we poll.
    fn run(&self, args: &[&str], timeout: Duration) -> Output {
        let stdout_path = self.dir.path().join("last.stdout");
        let stderr_path = self.dir.path().join("last.stderr");
        let stdout = fs::File::create(&stdout_path).expect("create stdout capture");
        let stderr = fs::File::create(&stderr_path).expect("create stderr capture");

        let mut child = self
            .command(args)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn concerto-cli");

        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().expect("poll child") {
                return Output {
                    status,
                    stdout: fs::read(&stdout_path).expect("read stdout capture"),
                    stderr: fs::read(&stderr_path).expect("read stderr capture"),
                };
            }
            if started.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "concerto run exceeded {timeout:?}\nstdout:\n{}\nstderr:\n{}",
                    read_lossy(&stdout_path),
                    read_lossy(&stderr_path)
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn read_lossy(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|_| "<unreadable>".to_string())
}

fn run_model(model: &str) {
    let fixture = Fixture::new(model);
    let prompt = "Read alpha.txt, beta.txt and gamma.txt. Then write report.md containing one \
                  line per file in the form `<name>: <FIRST WORD UPPERCASE>` (for example \
                  `alpha.txt: ALPHA`). Then read report.md back and confirm its contents.";
    let run = fixture.run(
        &["run", prompt, "--json", "--approvals", "auto", "--timeout", "300"],
        PROCESS_TIMEOUT,
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    eprintln!("model={model} run exit={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", run.status);

    let result: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
        panic!("model {model}: stdout is not one JSON object ({error})\nstdout:\n{stdout}\nstderr:\n{stderr}")
    });
    let status = result["status"].as_str().unwrap_or("<missing>");
    let session_id = result["session_id"].as_str().unwrap_or_default();
    assert!(
        status == "done" && !session_id.is_empty(),
        "model {model}: run did not finish cleanly (status={status}, session={session_id:?}) — \
         likely a persistent rate limit or provider error\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let cache = fixture.run(&["sessions", "cache", session_id, "--json"], Duration::from_secs(60));
    let cache_stdout = String::from_utf8_lossy(&cache.stdout);
    let cache_stderr = String::from_utf8_lossy(&cache.stderr);
    let summary: serde_json::Value =
        serde_json::from_str(cache_stdout.trim()).unwrap_or_else(|error| {
            panic!(
                "model {model}: sessions cache stdout is not one JSON object ({error})\nstdout:\n\
                 {cache_stdout}\nstderr:\n{cache_stderr}"
            )
        });
    eprintln!("model={model} cache summary:\n{}", cache_stdout.trim());

    let calls = summary["summary"]["cache"]["calls"].as_u64().unwrap_or(0);
    let calls_with_hits = summary["summary"]["cache"]["calls_with_hits"].as_u64().unwrap_or(0);
    let token_hit_rate = summary["token_hit_rate"].as_f64();
    // Best per-call reuse: the relay's upstream replicas write caches
    // independently, so the aggregate depends on routing; a single call that
    // reused a meaningful share of its prompt proves the stable prefix is
    // actually cacheable end to end.
    let best_call_rate = summary["attempts"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let prompt = call["usage"]["prompt_tokens"].as_u64()?;
                    let cached = call["usage"]["cache_read_tokens"].as_u64()?;
                    (prompt > 0).then_some(cached as f64 / prompt as f64)
                })
                .fold(0.0_f64, f64::max)
        })
        .unwrap_or(0.0);
    assert!(
        calls >= 2,
        "model {model}: fewer than two measured provider calls ({calls})\nstdout:\n{stdout}\n\
         stderr:\n{stderr}"
    );
    // DeepSeek's context cache is content-addressed, so the CLI run keeps the
    // strict 50% aggregate bar. GLM's anonymous free tier routes across relay
    // replicas whose upstream caches are not coordinated: the stable wire
    // session makes the relay's sticky provider selection deterministic, but a
    // cold prefix can still land on a replica that has not seen it. Require
    // real reuse on at least two calls plus one call reusing >=30% of its
    // prompt; the provider-level probe owns the strict >=50%
    // single-continuation guarantee with bounded retries.
    let min_calls_with_hits = if model == GLM { 2 } else { 1 };
    assert!(
        calls_with_hits >= min_calls_with_hits,
        "model {model}: fewer than {min_calls_with_hits} calls reported cached tokens \
         ({calls_with_hits})\ncache:\n{cache_stdout}"
    );
    if model == GLM {
        assert!(
            best_call_rate >= 0.3,
            "model {model}: best per-call cache reuse below 30% ({best_call_rate:.3}, \
             aggregate token hit rate {token_hit_rate:?})\ncache:\n{cache_stdout}"
        );
    } else {
        assert!(
            token_hit_rate.is_some_and(|rate| rate >= 0.5),
            "model {model}: token hit rate below 50% ({token_hit_rate:?})\ncache:\n{cache_stdout}"
        );
    }
}
