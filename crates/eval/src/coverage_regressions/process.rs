//! Real child-process coverage tests without installed coverage tools or global env mutation.

use crate::{EvalEngine, EvalError};
use concerto_config::ShellProfileConfig;
use concerto_core::types::CoverageTool;
use concerto_core::CancellationToken;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

const PASS: &str = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
const EMPTY: &str = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
const IGNORED: &str = "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

const CARGO: &str = r#"#!/bin/sh
tool="$1"
shift
printf '%s %s\n' "$tool" "$*" >> "$H16_ROOT/invocations"
if [ "${1-}" = "--version" ]; then
    if [ "${H16_MODE-}" = "probe-hang" ]; then
        printf ready > "$H16_ROOT/ready"
        exec sleep 30
    fi
    [ "$tool" = "$H16_TOOL" ] || exit 1
    printf '%s\n' "$tool fixture"
    exit 0
fi
[ "$tool" = "$H16_TOOL" ] || exit 64
if [ "${H16_MODE-}" = "run-hang" ]; then
    printf ready > "$H16_ROOT/ready"
    exec sleep 30
fi
report=''
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output-path) report="$2"; shift 2 ;;
        --output-dir) report="$2/tarpaulin-report.json"; shift 2 ;;
        *) shift ;;
    esac
done
printf '%s' "$H16_OUTPUT"
printf 'coverage diagnostic\n' >&2
# Keep even the unfixed implementation from writing reports outside this fixture.
case "$report" in
    "$H16_ROOT"/target/*|target/*) ;;
    *) exit 65 ;;
esac
case "${H16_MODE-}" in
    oversized) dd if=/dev/zero of="$report" bs=1048576 count=17 2>/dev/null; exit $? ;;
    directory) mkdir "$report"; exit $? ;;
    symlink) ln -s "$H16_ROOT/foreign-report.json" "$report"; exit $? ;;
esac
[ "${H16_MODE-}" = "missing" ] && exit 0
[ "${H16_MODE-}" = "failure" ] && exit 7
printf '%s' "$H16_REPORT" > "$report" || exit 66
"#;

fn fixture(tool: &str, output: &str) -> (tempfile::TempDir, EvalEngine) {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/harness-reliability/coverage-fixtures");
    std::fs::create_dir_all(&scratch).expect("disk-backed fixture directory");
    let dir = tempfile::Builder::new().prefix("coverage project ").tempdir_in(scratch).unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"coverage_fixture\"\nversion = \"0.0.0\"\n[workspace]\n",
    )
    .unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(&cargo, CARGO).unwrap();
    std::fs::set_permissions(cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let report = match tool {
        "llvm-cov" => r#"{"data":[{"totals":{"lines":{"count":4,"covered":3,"percent":75.0}}}]}"#,
        "tarpaulin" => r#"{"files":[],"coverage":75.0,"covered":3,"coverable":4}"#,
        _ => panic!("unsupported fixture tool"),
    };
    let profile = ShellProfileConfig {
        id: "coverage-fixture".into(),
        name: "Isolated coverage fixture".into(),
        executable: "/bin/sh".into(),
        path_additions: vec![bin],
        env: [
            ("H16_TOOL".into(), tool.into()),
            ("H16_OUTPUT".into(), output.into()),
            ("H16_REPORT".into(), report.into()),
            ("H16_ROOT".into(), dir.path().to_str().unwrap().into()),
        ]
        .into(),
        ..Default::default()
    };
    let engine =
        EvalEngine::with_timeout(dir.path(), Duration::from_secs(10)).with_shell_profile(profile);
    (dir, engine)
}

#[tokio::test]
async fn coverage_regression_profile_discovery_and_report_files_agree() {
    for (tool, expected) in
        [("llvm-cov", CoverageTool::LlvmCov), ("tarpaulin", CoverageTool::Tarpaulin)]
    {
        let (_dir, engine) = fixture(tool, PASS);
        let result = engine.run_with_coverage(CancellationToken::new()).await.unwrap();
        assert_eq!(result.exit_code, 0, "{tool}: {result:?}");
        assert!(result.passed, "{tool}: {result:?}");
        let coverage = result.coverage.expect("read the report file separately from diagnostics");
        assert_eq!(coverage.tool, expected);
        assert_eq!(coverage.line_percent, 75.0);
        assert!(result.output_tail.contains("coverage diagnostic"));
    }
}

#[tokio::test]
async fn coverage_regression_empty_and_ignored_suites_cannot_pass() {
    for (output, passed) in [(PASS, true), (EMPTY, false), (IGNORED, false)] {
        let (_dir, engine) = fixture("llvm-cov", output);
        let result = engine.run_with_coverage(CancellationToken::new()).await.unwrap();
        assert_eq!(result.exit_code, 0, "fixture command succeeds: {result:?}");
        assert_eq!(result.passed, passed, "{output}: {result:?}");
    }
}

#[tokio::test]
async fn coverage_regression_unicode_diagnostics_do_not_panic() {
    let output = format!("{PASS}{}a", "🙂".repeat(700));
    let (_dir, engine) = fixture("llvm-cov", &output);
    let result = engine.run_with_coverage(CancellationToken::new()).await.unwrap();
    assert!(result.passed, "{result:?}");
    assert!(result.output_tail.chars().count() <= 2000);
    assert!(result.output_tail.contains('🙂'));
    assert_eq!(result.coverage.expect("report").line_percent, 75.0);
}

fn set_env(engine: &mut EvalEngine, key: &str, value: &str) {
    engine.shell_profile.as_mut().unwrap().env.insert(key.into(), value.into());
}

fn assert_reports_removed(dir: &Path) {
    let target = dir.join("target");
    if target.exists() {
        assert_eq!(std::fs::read_dir(target).unwrap().count(), 0, "report directory leaked");
    }
}

#[tokio::test]
async fn coverage_regression_standalone_and_scoped_run_tests_once() {
    for tool in ["llvm-cov", "tarpaulin"] {
        for scoped in [false, true] {
            let (dir, engine) = fixture(tool, PASS);
            if scoped {
                let result = engine
                    .run_scoped_with_coverage(
                        &[camino::Utf8PathBuf::from("src/lib.rs")],
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                assert!(result.passed);
                assert_eq!(result.coverage.unwrap().line_percent, 75.0);
            } else {
                assert_eq!(
                    engine.run_coverage(CancellationToken::new()).await.unwrap().line_percent,
                    75.0
                );
            }
            let calls = std::fs::read_to_string(dir.path().join("invocations")).unwrap();
            assert_eq!(
                calls.lines().filter(|line| !line.ends_with("--version")).count(),
                1,
                "{calls}"
            );
            assert_reports_removed(dir.path());
        }
    }
}

#[tokio::test]
async fn coverage_regression_missing_malformed_and_stale_reports_fail() {
    for tool in ["llvm-cov", "tarpaulin"] {
        for malformed in [false, true] {
            let (dir, mut engine) = fixture(tool, PASS);
            if malformed {
                set_env(&mut engine, "H16_REPORT", "{}");
            } else {
                set_env(&mut engine, "H16_MODE", "missing");
            }
            // The previous fixed output location must never supply a report
            // for this run, even when a plausible measurement exists there.
            std::fs::create_dir(dir.path().join("target")).unwrap();
            let stale = dir.path().join("target/tarpaulin-report.json");
            std::fs::write(&stale, r#"{"coverage":99.0,"covered":99,"coverable":100}"#).unwrap();
            let error = engine.run_with_coverage(CancellationToken::new()).await.unwrap_err();
            assert!(
                matches!(error, EvalError::Io(_) | EvalError::HarnessSetupFailed(_)),
                "{error:?}"
            );
            assert!(stale.exists(), "another report must not be deleted");
            std::fs::remove_file(stale).unwrap();
            assert_reports_removed(dir.path());
        }
    }
}

#[tokio::test]
async fn coverage_regression_failed_commands_and_unexecuted_tests_stay_failed() {
    for tool in ["llvm-cov", "tarpaulin"] {
        let (dir, mut engine) = fixture(tool, PASS);
        set_env(&mut engine, "H16_MODE", "failure");
        let result = engine.run_with_coverage(CancellationToken::new()).await.unwrap();
        assert_eq!(result.exit_code, 7);
        assert!(!result.passed);
        assert!(result.coverage.is_none());
        assert_reports_removed(dir.path());
        assert!(matches!(
            engine.run_coverage(CancellationToken::new()).await,
            Err(EvalError::TestRunnerFailed(_))
        ));
        let (_dir, engine) = fixture(tool, EMPTY);
        assert!(matches!(
            engine.run_coverage(CancellationToken::new()).await,
            Err(EvalError::TestRunnerFailed(_))
        ));
    }
}

#[tokio::test]
async fn coverage_regression_cancelled_before_start_never_spawns() {
    let (dir, engine) = fixture("llvm-cov", PASS);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(engine.run_with_coverage(cancel).await, Err(EvalError::Cancelled)));
    assert!(!dir.path().join("invocations").exists());
    assert_reports_removed(dir.path());
}

#[tokio::test]
async fn coverage_regression_cancel_during_discovery_and_execution_propagates() {
    for mode in ["probe-hang", "run-hang"] {
        let (dir, mut engine) = fixture("llvm-cov", PASS);
        set_env(&mut engine, "H16_MODE", mode);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { engine.run_coverage(task_cancel).await });
        // Synchronize on the actual child entering the blocking phase.
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fixture reached blocking phase");
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert!(matches!(result, Err(EvalError::Cancelled)), "{mode}: {result:?}");
        assert_reports_removed(dir.path());
    }
}

#[tokio::test]
async fn coverage_regression_timeout_during_discovery_and_execution_propagates() {
    for mode in ["probe-hang", "run-hang"] {
        let (dir, mut engine) = fixture("llvm-cov", PASS);
        engine.timeout = Duration::from_secs(1);
        set_env(&mut engine, "H16_MODE", mode);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            engine.run_scoped_with_coverage(&[], CancellationToken::new()),
        )
        .await
        .expect("coverage timeout must bound the entire run");
        assert!(matches!(result, Err(EvalError::Timeout(_))), "{mode}: {result:?}");
        assert!(dir.path().join("ready").exists(), "fixture reached blocking phase");
        assert_reports_removed(dir.path());
    }
}

#[tokio::test]
async fn coverage_regression_oversized_and_nonregular_reports_are_rejected() {
    for mode in ["oversized", "directory", "symlink"] {
        let (dir, mut engine) = fixture("llvm-cov", PASS);
        set_env(&mut engine, "H16_MODE", mode);
        std::fs::write(
            dir.path().join("foreign-report.json"),
            r#"{"data":[{"totals":{"lines":{"count":4,"covered":3,"percent":75.0}}}]}"#,
        )
        .unwrap();
        let error = engine.run_with_coverage(CancellationToken::new()).await.unwrap_err();
        assert!(matches!(error, EvalError::HarnessSetupFailed(_)), "{mode}: {error:?}");
        assert_reports_removed(dir.path());
    }
}

#[tokio::test]
async fn coverage_regression_unsupported_project_never_probes_cargo() {
    let (dir, engine) = fixture("llvm-cov", PASS);
    std::fs::remove_file(dir.path().join("Cargo.toml")).unwrap();
    std::fs::write(dir.path().join("package.json"), "{}").unwrap();
    assert!(matches!(
        engine.run_with_coverage(CancellationToken::new()).await,
        Err(EvalError::HarnessUnavailable(_))
    ));
    assert!(!dir.path().join("invocations").exists());
    assert_reports_removed(dir.path());
}

#[tokio::test]
async fn coverage_regression_concurrent_runs_have_distinct_report_paths() {
    let (dir, engine) = fixture("llvm-cov", PASS);
    let (first, second) = tokio::join!(
        engine.run_with_coverage(CancellationToken::new()),
        engine.run_with_coverage(CancellationToken::new()),
    );
    assert!(first.unwrap().passed);
    assert!(second.unwrap().passed);
    let calls = std::fs::read_to_string(dir.path().join("invocations")).unwrap();
    let runs: Vec<_> = calls.lines().filter(|line| !line.ends_with("--version")).collect();
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0], runs[1], "reports must not be shared between runs");
    assert_reports_removed(dir.path());
}
