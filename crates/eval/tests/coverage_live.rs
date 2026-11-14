//! Optional released-tool contract check; fake-process regressions run by default.
//!
//! Set CONCERTO_COVERAGE_TOOLS_DIR to a directory containing
//! cargo-llvm-cov/cargo-llvm-cov and cargo-tarpaulin/cargo-tarpaulin, then run
//! cargo test -p concerto-eval --test coverage_live -- --ignored --nocapture.

#![cfg(unix)]

use concerto_config::ShellProfileConfig;
use concerto_core::types::CoverageTool;
use concerto_core::CancellationToken;
use concerto_eval::EvalEngine;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires released coverage binaries and llvm-tools-preview; see module instructions"]
async fn released_coverage_tools_run_real_tests_and_parse_real_reports() {
    let tools = std::env::var_os("CONCERTO_COVERAGE_TOOLS_DIR").expect("coverage tool directory");
    // Preserve the PATH Cargo launcher (normally rustup's proxy): Tarpaulin
    // invokes cargo +toolchain internally, which a toolchain's bare Cargo
    // binary from env!("CARGO") cannot interpret.
    let path = std::env::var_os("PATH").expect("Cargo launcher PATH");
    let cargo = std::env::split_paths(&path)
        .map(|directory| directory.join("cargo"))
        .find(|candidate| candidate.is_file())
        .expect("Cargo launcher");
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/harness-reliability/live-coverage-fixtures");
    std::fs::create_dir_all(&scratch).unwrap();
    for (tool, expected) in
        [("llvm-cov", CoverageTool::LlvmCov), ("tarpaulin", CoverageTool::Tarpaulin)]
    {
        let dir = tempfile::Builder::new().prefix("real coverage ").tempdir_in(&scratch).unwrap();
        let package = format!(
            "coverage_{}",
            concerto_core::ids::Ulid::new().to_string().to_ascii_lowercase()
        );
        std::fs::write(dir.path().join("Cargo.toml"), format!(
            "[package]\nname = \"{package}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n"
        )).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
pub fn classify(value: i32) -> i32 {
    if value > 0 {
        1
    } else {
        -1
    }
}
#[test]
fn checks_positive() {
    assert_eq!(classify(2), 1);
}
"#,
        )
        .unwrap();
        // LLVM avoids ptrace restrictions in containers; production honors
        // the project's own choice of engine/configuration.
        std::fs::write(dir.path().join(".tarpaulin.toml"), "[default]\nengine = \"Llvm\"\n")
            .unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let wrapper = bin.join("cargo");
        std::fs::write(
            &wrapper,
            r#"#!/bin/sh
case "$1" in
    llvm-cov|tarpaulin) [ "$1" = "$CONCERTO_FIXTURE_COVERAGE_TOOL" ] || exit 1 ;;
esac
exec "$CONCERTO_FIXTURE_REAL_CARGO" "$@"
"#,
        )
        .unwrap();
        std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profile = ShellProfileConfig {
            executable: "/bin/sh".into(),
            path_additions: vec![bin, Path::new(&tools).join(format!("cargo-{tool}"))],
            env: [
                ("CONCERTO_FIXTURE_COVERAGE_TOOL".into(), tool.into()),
                ("CONCERTO_FIXTURE_REAL_CARGO".into(), cargo.to_str().unwrap().into()),
                (
                    "CARGO_TARGET_DIR".into(),
                    dir.path().join("target/build").to_str().unwrap().into(),
                ),
            ]
            .into(),
            ..Default::default()
        };
        let result = EvalEngine::with_timeout(dir.path(), Duration::from_secs(120))
            .with_shell_profile(profile)
            .run_with_coverage(CancellationToken::new())
            .await
            .unwrap();
        assert!(result.passed, "{tool}: {result:?}");
        let coverage = result.coverage.expect("actual coverage report");
        assert_eq!(coverage.tool, expected);
        assert!(coverage.line_percent > 0.0 && coverage.line_percent <= 100.0, "{coverage:?}");
        assert!(result.output_tail.contains("1 passed"), "{}", result.output_tail);
        println!("{tool}: executed test passed; measured line coverage {}%", coverage.line_percent);
    }
}
