//! Regression coverage for honest Cargo verification (harness audit H04).

use std::path::Path;
use std::sync::Arc;

use camino::Utf8PathBuf;
use concerto_core::types::{EvalProvenance, ToolOutput};
use concerto_core::CancellationToken;
use concerto_eval::{EvalEngine, EvalError, EvalProcessExecutor};

const PASS: &str = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
const EMPTY: &str = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n";
const IGNORED: &str = "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
const FAIL: &str = "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

fn fixture() -> tempfile::TempDir {
    // Nested Cargo builds stay on disk, under the repository target directory.
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/harness-reliability/fixtures");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"h04_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn cargo_scope_does_not_hide_failing_unit_or_integration_tests() {
    let dir = fixture();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "mod foo;\n#[path = \"renamed.rs\"] mod actual_name;\n#[test] fn root_assertion() { panic!(\"root failure\"); }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.path().join("src/foo.rs"), "#[test] fn smoke() {}\n").unwrap();
    std::fs::write(
        dir.path().join("src/renamed.rs"),
        "#[test] fn renamed_assertion() { panic!(\"renamed failure\"); }\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("tests")).unwrap();
    std::fs::write(
        dir.path().join("tests/behavior.rs"),
        "#[test] fn behavior() { panic!(\"integration failure\"); }\n",
    )
    .unwrap();
    let engine = EvalEngine::new(dir.path());
    for paths in [
        vec!["src/lib.rs"],
        vec!["src/main.rs"],
        vec!["src/renamed.rs"],
        vec!["src/foo.rs", "src/renamed.rs"],
        vec!["src/foo.rs"],
    ] {
        let changed: Vec<_> = paths.iter().map(|path| Utf8PathBuf::from(*path)).collect();
        let result = engine.run_scoped(&changed, CancellationToken::new()).await.unwrap();
        assert!(!result.passed, "{paths:?} hid failures: {result:?}");
        assert_ne!(result.exit_code, 0, "{paths:?} must actually run failing tests");
    }
    // A passing unit suite must not hide unrelated failing integration tests.
    std::fs::write(dir.path().join("src/lib.rs"), "mod foo;\n").unwrap();
    let result = engine
        .run_scoped(&[Utf8PathBuf::from("src/foo.rs")], CancellationToken::new())
        .await
        .unwrap();
    assert!(!result.passed, "integration failures must participate: {result:?}");
    assert!(result.output_tail.contains("integration failure"));
}

#[tokio::test]
async fn cargo_requires_executed_tests_in_direct_and_fallback_runs() {
    let dir = fixture();
    let nested = dir.path().join("no-manifest");
    std::fs::create_dir(&nested).unwrap();
    for (source, expected) in [
        ("pub fn f() {}\n", false),
        ("#[test] #[ignore] fn skipped() {}\n", false),
        ("#[test] fn works() {}\n", true),
        ("/// ```\n/// assert_eq!(1 + 1, 2);\n/// ```\npub fn documented() {}\n", true),
    ] {
        std::fs::write(dir.path().join("src/lib.rs"), source).unwrap();
        for (root, provenance) in
            [(dir.path(), EvalProvenance::Harness), (nested.as_path(), EvalProvenance::Fallback)]
        {
            let result = EvalEngine::new(root).run(CancellationToken::new()).await.unwrap();
            assert_eq!(result.exit_code, 0, "Cargo itself succeeds: {result:?}");
            assert_eq!(result.passed, expected, "{source}: {result:?}");
            assert_eq!(result.provenance, provenance);
            if !expected {
                assert!(result.output_tail.contains("Rust test success was not confirmed"));
            }
        }
    }
}

struct RecordedOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

#[async_trait::async_trait]
impl EvalProcessExecutor for RecordedOutput {
    async fn execute(
        &self,
        program: &str,
        args: &[&str],
        _cwd: &Path,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, EvalError> {
        assert_eq!(program, "cargo");
        assert_eq!(args, ["test"], "do not guess test filters from source paths");
        assert!(!cancel.is_cancelled());
        Ok(ToolOutput {
            summary: "fixture".into(),
            data: serde_json::json!({
                "exit_code": self.exit_code, "stdout": self.stdout, "stderr": self.stderr,
            }),
        })
    }
}

#[tokio::test]
async fn cargo_governed_results_require_evidence_before_truncating_output() {
    let dir = fixture();
    for (stdout, stderr, exit_code, expected) in [
        (EMPTY.to_owned(), "".into(), 0, false),
        (IGNORED.to_owned(), "".into(), 0, false),
        ("".into(), "".into(), 0, false),
        ("custom harness: success".into(), "".into(), 0, false),
        ("test result: ok. broken passed; 0 failed;".into(), "".into(), 0, false),
        (PASS.into(), "".into(), 1, false),
        (format!("{PASS}{FAIL}"), "".into(), 0, false),
        ("test result: ok. 1 passed; 1 failed;".into(), "".into(), 0, false),
        (format!("{PASS}test result: ok. 1 passed;"), "".into(), 0, false),
        (format!("{PASS}test result: ok. 1 passed; 0 failed;"), "".into(), 0, false),
        (PASS.replace("0 ignored", "bad ignored"), "".into(), 0, false),
        (PASS.replace("0.00s", "NaNs"), "".into(), 0, false),
        (format!("{PASS}{EMPTY}"), "".into(), 0, true),
        (format!("{EMPTY}{PASS}"), "".into(), 0, true),
        ("".into(), PASS.into(), 0, true),
        (format!("{PASS}{}", "🙂".repeat(2100)), "".into(), 0, true),
    ] {
        let engine = EvalEngine::new(dir.path()).with_process_executor(Arc::new(RecordedOutput {
            exit_code,
            stdout,
            stderr,
        }));
        let result = engine.run(CancellationToken::new()).await.unwrap();
        assert_eq!(result.passed, expected, "{result:?}");
        assert_eq!(result.exit_code, exit_code);
        assert!(result.output_tail.chars().count() <= 2000);
    }
}
