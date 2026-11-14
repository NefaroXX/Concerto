//! Regression inputs follow the released coverage tools' report contracts.

use super::EvalEngine;
use concerto_core::types::CoverageTool;

#[cfg(unix)]
mod process;

#[test]
fn coverage_regression_tarpaulin_reads_released_report_fields() {
    // Tarpaulin 0.37.5 src/report/json.rs emits a percentage, not a fraction,
    // under `coverage`, alongside aggregate covered/coverable counts.
    let raw = r#"{"files":[],"coverage":82.5,"covered":33,"coverable":40}"#;
    let result = EvalEngine::parse_tarpaulin_json(raw).expect("released report shape");
    assert_eq!(result.tool, CoverageTool::Tarpaulin);
    assert!((result.line_percent - 82.5).abs() < f64::EPSILON);
    assert!(result.function_percent.is_none());
    assert!(result.branch_percent.is_none());
}

#[test]
fn coverage_regression_tarpaulin_distinguishes_zero_from_missing_measurement() {
    let zero = r#"{"files":[],"coverage":0.0,"covered":0,"coverable":40}"#;
    assert_eq!(EvalEngine::parse_tarpaulin_json(zero).expect("measured zero").line_percent, 0.0);
    for raw in ["{}", r#"{"files":[]}"#, r#"{"covered":0,"coverable":40}"#] {
        assert!(EvalEngine::parse_tarpaulin_json(raw).is_none(), "missing measurement: {raw}");
    }
}

#[test]
fn coverage_regression_tarpaulin_rejects_impossible_measurements() {
    for raw in [
        r#"{"coverage":-1.0,"covered":0,"coverable":40}"#,
        r#"{"coverage":101.0,"covered":40,"coverable":40}"#,
        r#"{"coverage":82.5,"covered":41,"coverable":40}"#,
    ] {
        assert!(EvalEngine::parse_tarpaulin_json(raw).is_none(), "invalid measurement: {raw}");
    }
}

#[test]
fn coverage_regression_llvm_rejects_out_of_range_percentages() {
    for metric in ["lines", "functions", "branches"] {
        for percent in [-0.5, 100.5] {
            let mut report = serde_json::json!({"data":[{"totals":{
                "lines":{"count":40,"covered":33,"percent":82.5},
                "functions":{"count":10,"covered":10,"percent":100.0},
                "branches":{"count":2,"covered":0,"percent":0.0}
            }}]});
            report["data"][0]["totals"][metric]["percent"] = serde_json::json!(percent);
            assert!(
                EvalEngine::parse_llvm_cov_json(&report.to_string()).is_none(),
                "invalid {metric} percentage: {percent}",
            );
        }
    }
}

#[test]
fn coverage_regression_llvm_rejects_missing_and_impossible_counts() {
    for metric in [
        serde_json::json!({"percent":75.0}),
        serde_json::json!({"percent":75.0,"count":4,"covered":5}),
        serde_json::json!({"percent":75.0,"count":-1,"covered":0}),
    ] {
        let raw = serde_json::json!({"data":[{"totals":{"lines":metric}}]}).to_string();
        assert!(EvalEngine::parse_llvm_cov_json(&raw).is_none(), "{raw}");
    }
}

#[test]
fn coverage_regression_llvm_does_not_silently_select_first_dataset() {
    let data = serde_json::json!({"totals":{"lines":{"count":4,"covered":3,"percent":75.0}}});
    let raw = serde_json::json!({"data":[data.clone(), data]}).to_string();
    assert!(EvalEngine::parse_llvm_cov_json(&raw).is_none());
}
