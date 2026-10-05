#!/usr/bin/env bash
#
# Q03 — contract-test harness for the Q02 mutation gate runner.
#
# Contract: CX/20261005-q01-v1/001 (accepted Q01 v1, part 5: controlled
#           Rust fixtures prove caught/missed and exercise every failure class)
# Task:     Q03 — writer OpenCode-solo (transfer; Q02 writer OpenCode)
# Claim:    OpenCode/sol-20261005-01/Q03/001
# Scope:    new files under tests/mutation_gate/** only. The runner under test
#           (scripts/mutation-gate.sh, Q02 head 05e24f6) is read-only input.
#
# The harness builds disposable git fixture workspaces under --work-dir that
# mirror the Q01 pilot paths (crates/core/src/policy.rs,
# crates/core/src/shell_security.rs, crates/shell/src/parser.rs) and the
# package names concerto-core / concerto-shell, then drives the runner with
# explicit --base/--report-dir/--scratch-dir and asserts:
#   * the process exit code of each contract case,
#   * gate-report.json status/coverage keys, and
#   * that the candidate checkout is never modified by the runner.
#
# Real cargo-mutants 27.1.0 proves the caught/missed/baseline/deadline/
# cancellation cases. Shims are used ONLY for setup/artifact/exit edges
# (wrong version pin, malformed artifacts) and are labeled [simulated] in the
# logs; they never replace the real caught/missed evidence.
#
# Usage:
#   bash tests/mutation_gate/run-contract-tests.sh \
#       --runner <absolute Q02 script> --work-dir <disk-backed DIR>
#   bash tests/mutation_gate/run-contract-tests.sh --help
#
# Exit codes:
#   0  every contract case passed
#   1  setup_error (usage, path validation, host tools, pinned tool missing)
#   2  one or more contract cases failed
#
# No production-only bypass flags exist: the pinned tool and an explicit
# runner/work-dir are mandatory.
set -euo pipefail

readonly CONTRACT_ID="CX/20261005-q01-v1/001"
readonly CLAIM_TOKEN="OpenCode/sol-20261005-01/Q03/001"
readonly MUTANTS_PIN="27.1.0"

RUNNER=""
WORK_DIR=""
SESSION_DIR=""
REPORT_FILE=""
HARNESS_SELF=""
HARNESS_DIR=""
REPO_ROOT=""
PINNED_TOOL=""
RUNNER_SHA256=""
RUNNER_GIT_HEAD="n/a"
RUNNER_GIT_BLOB="n/a"
START_SECONDS=0

# Per-case state (managed by run_case).
CASE_NAME=""
CASE_GROUP=""
CASE_DIR=""
SOURCE_SNAPSHOT=""
RUN_RC=""
RUN_OUT=""
SPAWN_RC=""
SPAWN_OUT=""
STANDARD_BASE=""

##############################################################################
# Usage / argument parsing
##############################################################################

usage() {
    cat <<EOF
Q03 contract-test harness for the Q02 mutation gate runner ($CONTRACT_ID).

Usage:
  bash tests/mutation_gate/run-contract-tests.sh --runner <ABSOLUTE Q02 script> \\
      --work-dir <disk-backed DIR>
  bash tests/mutation_gate/run-contract-tests.sh --help

Required (no defaults, no bypass flags):
  --runner <PATH>      Absolute path to scripts/mutation-gate.sh (Q02 head).
  --work-dir <DIR>     Absolute disk-backed directory that receives disposable
                       fixture workspaces, reports and scratch dirs. Must be
                       outside the production checkout; /tmp, /dev/shm,
                       in-repo and repo-ancestor paths are rejected with
                       setup_error (exit 1).
  --help               Show this help and exit 0.

Behavior:
  One disposable git fixture workspace per case (deterministic baseline +
  candidate commits, Q01 pilot paths, packages concerto-core/concerto-shell).
  Real cargo-mutants $MUTANTS_PIN cases prove caught/missed/baseline/deadline/
  cancellation; shims labeled [simulated] cover only wrong-pin and malformed
  artifact edges. Each case asserts exit code, gate-report.json status and
  coverage keys, and that the candidate source is unchanged.

Exit codes:
  0  every case passed
  1  setup_error (usage, paths, host tools, pinned tool missing)
  2  one or more contract cases failed
EOF
}

parse_args() {
    local have_runner=0 have_work=0
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --help | -h)
                usage
                exit 0
                ;;
            --runner | --work-dir)
                if [ "$#" -lt 2 ]; then
                    setup_error "missing value for $1"
                fi
                case "$2" in
                    --*) setup_error "missing value for $1" ;;
                esac
                if [ "$1" = "--runner" ]; then
                    RUNNER="$2"
                    have_runner=1
                else
                    WORK_DIR="$2"
                    have_work=1
                fi
                shift 2
                ;;
            --runner=*)
                RUNNER="${1#--runner=}"
                have_runner=1
                shift
                ;;
            --work-dir=*)
                WORK_DIR="${1#--work-dir=}"
                have_work=1
                shift
                ;;
            --*)
                setup_error "unknown option: $1"
                ;;
            *)
                setup_error "unexpected positional argument: $1"
                ;;
        esac
    done
    if [ "$have_runner" -ne 1 ] || [ -z "$RUNNER" ]; then
        setup_error "missing required argument: --runner <absolute Q02 script> (no default)"
    fi
    if [ "$have_work" -ne 1 ] || [ -z "$WORK_DIR" ]; then
        setup_error "missing required argument: --work-dir <disk-backed DIR> (no default)"
    fi
}

##############################################################################
# Validation (every failure exits 1 with a setup_error label)
##############################################################################

validate_runner() {
    case "$RUNNER" in
        /*) ;;
        *) setup_error "--runner must be an absolute path (got: $RUNNER)" ;;
    esac
    RUNNER="$(absp "$RUNNER")"
    if [ ! -f "$RUNNER" ]; then
        setup_error "--runner file not found: $RUNNER"
    fi
    if [ ! -r "$RUNNER" ]; then
        setup_error "--runner file not readable: $RUNNER"
    fi
}

validate_work_dir() {
    case "$WORK_DIR" in
        /*) ;;
        *) setup_error "--work-dir must be an absolute path (got: $WORK_DIR)" ;;
    esac
    WORK_DIR="$(absp "$WORK_DIR")"
    case "$WORK_DIR" in
        /tmp | /tmp/*)
            setup_error "work_dir_not_allowed: $WORK_DIR must not live under /tmp"
            ;;
        /dev/shm | /dev/shm/*)
            setup_error "work_dir_not_allowed: $WORK_DIR must not live under /dev/shm"
            ;;
    esac
    if [ "$WORK_DIR" = "$REPO_ROOT" ]; then
        setup_error "work_dir_is_checkout: $WORK_DIR is the production checkout"
    fi
    if path_within "$WORK_DIR" "$REPO_ROOT"; then
        setup_error "work_dir_inside_checkout: $WORK_DIR is inside $REPO_ROOT"
    fi
    if path_within "$REPO_ROOT" "$WORK_DIR"; then
        setup_error "work_dir_contains_checkout: $WORK_DIR contains $REPO_ROOT"
    fi
    if [ -e "$WORK_DIR" ] && [ ! -d "$WORK_DIR" ]; then
        setup_error "work_dir_not_a_directory: $WORK_DIR"
    fi
    if ! mkdir -p "$WORK_DIR" 2>/dev/null; then
        setup_error "work_dir_unusable: cannot create $WORK_DIR"
    fi
    local fst
    fst="$(fs_type "$WORK_DIR")"
    case "$fst" in
        tmpfs | ramfs) setup_error "work_dir_on_tmpfs: $WORK_DIR is on $fst" ;;
    esac
}

preflight_host_tools() {
    local t
    for t in git python3 setsid timeout sha256sum grep awk sed tr stat; do
        if ! command -v "$t" >/dev/null 2>&1; then
            setup_error "host_tool_not_found: $t is required"
        fi
    done
    if ! REPO_ROOT="$(git -C "$HARNESS_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
        setup_error "harness_not_in_checkout: cannot resolve the git checkout containing $HARNESS_DIR"
    fi
}

# The real cases need the pinned tool before any case runs; there is no
# bypass flag — a missing pin is a setup_error.
preflight_pinned_tool() {
    local probe token cand bin
    local -a bins=()
    if command -v cargo-mutants >/dev/null 2>&1; then
        bins+=("$(command -v cargo-mutants)")
    fi
    bin="${CARGO_HOME:-${HOME:-}/.cargo}/bin/cargo-mutants"
    if [ -x "$bin" ]; then
        bins+=("$bin")
    fi
    for cand in "${bins[@]}"; do
        probe="$("$cand" mutants --version 2>&1)" || true
        token="$(printf '%s' "$probe" | awk 'END { print $NF }')"
        if [ "$token" = "$MUTANTS_PIN" ]; then
            PINNED_TOOL="$cand mutants"
            return 0
        fi
        probe="$("$cand" --version 2>&1)" || true
        token="$(printf '%s' "$probe" | awk 'END { print $NF }')"
        if [ "$token" = "$MUTANTS_PIN" ]; then
            PINNED_TOOL="$cand"
            return 0
        fi
    done
    if command -v cargo >/dev/null 2>&1; then
        probe="$(cargo mutants --version 2>&1)" || true
        token="$(printf '%s' "$probe" | awk 'END { print $NF }')"
        if [ "$token" = "$MUTANTS_PIN" ]; then
            PINNED_TOOL="cargo mutants"
            return 0
        fi
    fi
    setup_error "cargo_mutants_pin_unsatisfied: real cases need cargo-mutants $MUTANTS_PIN; install with: cargo install cargo-mutants --version $MUTANTS_PIN --locked"
}

publish_identity() {
    local top rel
    RUNNER_SHA256="$(sha256sum "$RUNNER" | awk '{print $1}')"
    if top="$(git -C "$(dirname "$RUNNER")" rev-parse --show-toplevel 2>/dev/null)"; then
        RUNNER_GIT_HEAD="$(git -C "$top" rev-parse HEAD)"
        rel="${RUNNER#"$top"/}"
        RUNNER_GIT_BLOB="$(git -C "$top" rev-parse "HEAD:$rel" 2>/dev/null || printf 'n/a')"
    else
        RUNNER_GIT_HEAD="standalone (not inside a git work tree)"
    fi
    printf 'contract: %s\n' "$CONTRACT_ID"
    printf 'claim: %s\n' "$CLAIM_TOKEN"
    printf 'checkout-head: %s\n' "$(git -C "$REPO_ROOT" rev-parse HEAD)"
    printf 'runner: %s\n' "$RUNNER"
    printf 'runner-sha256: %s\n' "$RUNNER_SHA256"
    printf 'runner-git-head: %s\n' "$RUNNER_GIT_HEAD"
    printf 'runner-git-blob: %s\n' "$RUNNER_GIT_BLOB"
    printf 'pinned-cargo-mutants: %s\n' "$PINNED_TOOL"
    printf 'work-dir: %s\n' "$WORK_DIR"
}

##############################################################################
# Case runner
##############################################################################

run_case() {
    local name="$1" group="$2" func="$3"
    CASE_NAME="$name"
    CASE_GROUP="$group"
    CASE_DIR="$SESSION_DIR/cases/$name"
    if ! mkdir -p "$CASE_DIR"; then
        printf '\n=== case %s (group: %s) ===\nFAILED to create %s\n' "$name" "$group" "$CASE_DIR"
        CASE_RESULTS+=("$group|$name|FAIL")
        GROUP_FAILED["$group"]=1
        return 0
    fi
    CASE_FAILURES=()
    REPORT_FILE="$CASE_DIR/report/gate-report.json"
    RUN_RC=""
    RUN_OUT="$CASE_DIR/runner.out"
    SOURCE_SNAPSHOT=""
    SPAWN_RC=""
    SPAWN_OUT="$CASE_DIR/spawn.out"

    printf '\n=== case %s (group: %s) ===\n' "$name" "$group"
    local rc=0
    if "$func" >"$CASE_DIR/case.log" 2>&1; then
        :
    else
        rc=$?
    fi
    cat "$CASE_DIR/case.log"
    if [ "$rc" -ne 0 ]; then
        case_fail "case aborted with exit $rc (see $CASE_DIR/case.log)"
    fi

    local verdict=PASS
    if [ "${#CASE_FAILURES[@]}" -gt 0 ]; then
        verdict=FAIL
        GROUP_FAILED["$group"]=1
        local f
        for f in "${CASE_FAILURES[@]}"; do
            printf '  FAIL: %s\n' "$f"
        done
    fi
    printf 'case-result: %s %s %s\n' "$group" "$name" "$verdict"
    CASE_RESULTS+=("$group|$name|$verdict")
    local g seen=0
    for g in "${GROUP_ORDER[@]+"${GROUP_ORDER[@]}"}"; do
        if [ "$g" = "$group" ]; then
            seen=1
            break
        fi
    done
    if [ "$seen" -eq 0 ]; then
        GROUP_ORDER+=("$group")
    fi
    return 0
}

##############################################################################
# Entry-contract cases (harness validates its own interface)
##############################################################################

case_entry_help() {
    spawn_harness --help
    expect_eq "$SPAWN_RC" "0" "--help exit code"
    local out
    out="$(spawn_output)"
    expect_contains "$out" "--runner" "help documents --runner"
    expect_contains "$out" "--work-dir" "help documents --work-dir"
    return 0
}

case_entry_required_args() {
    spawn_harness
    expect_eq "$SPAWN_RC" "1" "no-argument invocation exit code"
    local out
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "no-argument error label"
    expect_contains "$out" "--runner" "no-argument error names --runner"

    spawn_harness --runner "$RUNNER"
    expect_eq "$SPAWN_RC" "1" "missing --work-dir exit code"
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "missing --work-dir error label"
    expect_contains "$out" "--work-dir" "missing --work-dir error names --work-dir"

    spawn_harness --bogus-flag
    expect_eq "$SPAWN_RC" "1" "unknown option exit code"
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "unknown option error label"
    return 0
}

case_entry_runner_rejects() {
    spawn_harness --runner scripts/mutation-gate.sh --work-dir "$WORK_DIR"
    expect_eq "$SPAWN_RC" "1" "relative --runner exit code"
    local out
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "relative --runner error label"
    expect_contains "$out" "absolute" "relative --runner message"

    spawn_harness --runner /nonexistent/q03/mutation-gate.sh --work-dir "$WORK_DIR"
    expect_eq "$SPAWN_RC" "1" "missing --runner file exit code"
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "missing --runner file error label"
    expect_contains "$out" "not found" "missing --runner file message"
    return 0
}

case_entry_workdir_rejects() {
    local out probe

    probe="/tmp/q03-entry-reject-$$"
    spawn_harness --runner "$RUNNER" --work-dir "$probe"
    expect_eq "$SPAWN_RC" "1" "/tmp work-dir exit code"
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "/tmp work-dir error label"
    expect_contains "$out" "/tmp" "/tmp work-dir message"
    if [ -e "$probe" ]; then
        case_fail "rejected /tmp work-dir was created: $probe"
    fi

    probe="/dev/shm/q03-entry-reject-$$"
    spawn_harness --runner "$RUNNER" --work-dir "$probe"
    expect_eq "$SPAWN_RC" "1" "/dev/shm work-dir exit code"
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "/dev/shm work-dir error label"
    expect_contains "$out" "/dev/shm" "/dev/shm work-dir message"
    if [ -e "$probe" ]; then
        case_fail "rejected /dev/shm work-dir was created: $probe"
    fi

    probe="$REPO_ROOT/tests/mutation_gate/entry-reject-$$"
    spawn_harness --runner "$RUNNER" --work-dir "$probe"
    expect_eq "$SPAWN_RC" "1" "in-repo work-dir exit code"
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "in-repo work-dir error label"
    expect_contains "$out" "inside" "in-repo work-dir message"
    if [ -e "$probe" ]; then
        case_fail "rejected in-repo work-dir was created: $probe"
    fi

    spawn_harness --runner "$RUNNER" --work-dir "$REPO_ROOT"
    expect_eq "$SPAWN_RC" "1" "checkout-as-work-dir exit code"
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "checkout-as-work-dir error label"
    expect_contains "$out" "production checkout" "checkout-as-work-dir message"

    spawn_harness --runner "$RUNNER" --work-dir "$(dirname "$REPO_ROOT")"
    expect_eq "$SPAWN_RC" "1" "repo-ancestor work-dir exit code"
    out="$(spawn_output)"
    expect_contains "$out" "setup_error" "repo-ancestor work-dir error label"
    expect_contains "$out" "contains" "repo-ancestor work-dir message"
    return 0
}

##############################################################################
# Real cargo-mutants 27.1.0 cases (no shims)
##############################################################################

# Shared fixture: weak baseline -> strong candidate (policy.rs changes).
fixture_standard_candidate() {
    local repo="$1"
    fixture_new "$repo" || return 1
    fixture_commit "$repo" "baseline" || return 1
    STANDARD_BASE="$(fixture_head "$repo")"
    fixture_write_policy "$repo" strong || return 1
    fixture_commit "$repo" "candidate: strongly asserted classify boundaries" || return 1
    return 0
}

case_real_caught() {
    local repo="$CASE_DIR/repo"
    fixture_standard_candidate "$repo" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    invoke_runner "$repo" "$STANDARD_BASE" 900 600

    expect_eq "$RUN_RC" "0" "runner exit code (caught)"
    expect_report_eq status "passed"
    expect_report_eq reason "all_mutants_caught"
    expect_report_eq exit_code "0"
    expect_report_eq complete "true"
    expect_report_eq coverage_scope "pilot_only"
    expect_report_eq tool_version "$MUTANTS_PIN"
    expect_report_eq contract_id "$CONTRACT_ID"
    expect_report_eq selected_files '["crates/core/src/policy.rs"]'
    expect_report_eq counts.missed "0"
    expect_report_eq counts.timeout "0"
    expect_report_ge counts.caught 1
    assert_source_unchanged "$repo"
    return 0
}

case_real_missed() {
    local repo="$CASE_DIR/repo"
    fixture_new "$repo" || return 1
    fixture_write_policy "$repo" strong || return 1
    fixture_commit "$repo" "baseline" || return 1
    local base
    base="$(fixture_head "$repo")"
    fixture_write_policy "$repo" weak || return 1
    fixture_commit "$repo" "candidate: weak baseline-passing test" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    invoke_runner "$repo" "$base" 900 600

    expect_eq "$RUN_RC" "2" "runner exit code (missed)"
    expect_report_eq status "missed"
    expect_report_contains reason "missed_mutants"
    expect_report_eq exit_code "2"
    expect_report_eq complete "true"
    expect_report_eq tool_version "$MUTANTS_PIN"
    expect_report_contains selected_files "crates/core/src/policy.rs"
    expect_report_ge counts.missed 1
    assert_source_unchanged "$repo"
    return 0
}

case_baseline_failure() {
    local repo="$CASE_DIR/repo"
    fixture_new "$repo" || return 1
    fixture_write_policy "$repo" strong || return 1
    fixture_commit "$repo" "baseline" || return 1
    local base
    base="$(fixture_head "$repo")"
    fixture_write_policy "$repo" broken || return 1
    fixture_commit "$repo" "candidate: baseline tests fail" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    invoke_runner "$repo" "$base" 600 300

    expect_eq "$RUN_RC" "4" "runner exit code (baseline failure)"
    expect_report_eq status "baseline_failed"
    expect_report_eq reason "baseline_tests_failed (exit 4)"
    expect_report_eq exit_code "4"
    expect_report_eq complete "true"
    assert_source_unchanged "$repo"
    return 0
}

case_deadline_budget() {
    local repo="$CASE_DIR/repo"
    fixture_new "$repo" || return 1
    fixture_commit "$repo" "baseline" || return 1
    local base
    base="$(fixture_head "$repo")"
    fixture_write_policy "$repo" slow || return 1
    fixture_commit "$repo" "candidate: slow strongly-asserted tests" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    # Real tool, 1s wall-clock budget: the run must be cut off as inconclusive.
    invoke_runner "$repo" "$base" 300 1

    expect_eq "$RUN_RC" "3" "runner exit code (deadline)"
    expect_report_eq status "inconclusive"
    expect_report_eq reason "budget_exceeded"
    expect_report_eq exit_code "3"
    expect_report_eq complete "false"
    expect_report_eq budget_seconds "1"
    expect_file_contains "$RUN_OUT" "budget of 1s exceeded"
    assert_source_unchanged "$repo"
    return 0
}

case_signal_cancel() {
    local repo="$CASE_DIR/repo"
    fixture_new "$repo" || return 1
    fixture_commit "$repo" "baseline" || return 1
    local base
    base="$(fixture_head "$repo")"
    fixture_write_policy "$repo" slow || return 1
    fixture_commit "$repo" "candidate: slow strongly-asserted tests" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"

    if ! invoke_runner_signal "$repo" "$base" TERM; then
        case_fail "cancellation window never opened: tool run did not start"
        return 0
    fi
    expect_eq "$RUN_RC" "143" "runner exit code after SIGTERM"
    expect_report_eq status "inconclusive"
    expect_report_contains reason "external_cancelled"
    expect_report_contains reason "SIGTERM"
    expect_report_eq exit_code "143"
    expect_report_eq complete "false"
    expect_file_contains "$RUN_OUT" "interrupted by SIGTERM"
    assert_source_unchanged "$repo"
    return 0
}

##############################################################################
# Diff-classification and setup cases (tool runs skipped or never reached)
##############################################################################

case_empty_diff() {
    local repo="$CASE_DIR/repo"
    fixture_new "$repo" || return 1
    fixture_commit "$repo" "baseline" || return 1
    local base
    base="$(fixture_head "$repo")"
    fixture_commit_empty "$repo" "candidate: no changes" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    invoke_runner "$repo" "$base" 300 120

    expect_eq "$RUN_RC" "0" "runner exit code (empty diff)"
    expect_report_eq status "skipped_no_rust_changes"
    expect_report_eq reason "no_rust_changes_since_merge_base"
    expect_report_eq exit_code "0"
    expect_report_eq complete "true"
    expect_report_eq selected_files "[]"
    expect_report_eq uncovered_files "[]"
    assert_source_unchanged "$repo"
    return 0
}

case_out_of_pilot_diff() {
    local repo="$CASE_DIR/repo"
    fixture_new "$repo" || return 1
    fixture_commit "$repo" "baseline" || return 1
    local base
    base="$(fixture_head "$repo")"
    fixture_write_lib_candidate "$repo" || return 1
    fixture_commit "$repo" "candidate: non-pilot .rs change" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    invoke_runner "$repo" "$base" 300 120

    expect_eq "$RUN_RC" "0" "runner exit code (out-of-pilot)"
    expect_report_eq status "skipped_out_of_pilot"
    expect_report_contains reason "rust_changes_present"
    expect_report_eq exit_code "0"
    expect_report_eq complete "true"
    expect_report_eq coverage_scope "pilot_only"
    expect_report_eq selected_files "[]"
    expect_report_contains uncovered_files "crates/core/src/lib.rs"
    assert_source_unchanged "$repo"
    return 0
}

case_invalid_base() {
    local repo="$CASE_DIR/repo"
    fixture_standard_candidate "$repo" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    invoke_runner "$repo" "no-such-ref-q03" 300 120

    expect_eq "$RUN_RC" "1" "runner exit code (invalid base)"
    expect_report_eq status "setup_error"
    expect_report_contains reason "base_ref_invalid"
    expect_report_eq exit_code "1"
    expect_report_eq complete "false"
    assert_source_unchanged "$repo"
    return 0
}

case_wrong_pin() {
    local repo="$CASE_DIR/repo"
    fixture_standard_candidate "$repo" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    mkdir -p "$CASE_DIR/empty-cargo-home" || return 1
    printf '[simulated] wrong-pin case: fake cargo-mutants (26.0.0) on PATH, empty CARGO_HOME\n'
    invoke_runner "$repo" "$STANDARD_BASE" 300 120 \
        "PATH=$HARNESS_DIR/shims/wrong-version:$PATH" \
        "CARGO_HOME=$CASE_DIR/empty-cargo-home"

    expect_eq "$RUN_RC" "1" "runner exit code (wrong pin)"
    expect_report_eq status "setup_error"
    expect_report_contains reason "cargo_mutants_version_mismatch"
    expect_report_eq exit_code "1"
    expect_report_eq complete "false"
    assert_source_unchanged "$repo"
    return 0
}

case_odd_names_rename_delete() {
    local repo="$CASE_DIR/repo"
    fixture_new "$repo" || return 1
    fixture_commit "$repo" "baseline" || return 1
    local base
    base="$(fixture_head "$repo")"
    git -C "$repo" mv crates/shell/src/parser.rs crates/shell/src/parser_renamed.rs || return 1
    git -C "$repo" rm -q crates/core/src/policy.rs || return 1
    cat >"$repo/crates/core/src/lib.rs" <<'EOF'
pub mod shell_security;
EOF
    mkdir -p "$repo/docs" || return 1
    printf 'odd filenames fixture\n' >"$repo/docs/odd names and ünïcode.md" || return 1
    printf '// non-pilot source file with a space in its name\n' \
        >"$repo/crates/core/src/odd name.rs" || return 1
    fixture_commit "$repo" "candidate: rename, deletion, odd names" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    invoke_runner "$repo" "$base" 300 120

    expect_eq "$RUN_RC" "0" "runner exit code (odd names)"
    expect_report_eq status "skipped_out_of_pilot"
    expect_report_eq exit_code "0"
    expect_report_eq complete "true"
    expect_report_eq selected_files "[]"
    expect_report_contains uncovered_files "crates/shell/src/parser_renamed.rs"
    expect_report_contains uncovered_files "crates/core/src/policy.rs"
    expect_report_contains uncovered_files "crates/core/src/lib.rs"
    expect_report_contains uncovered_files "crates/core/src/odd name.rs"
    expect_report_contains skipped_files "docs/odd names and ünïcode.md"
    assert_source_unchanged "$repo"
    return 0
}

case_dirty_worktree() {
    local repo="$CASE_DIR/repo"
    fixture_standard_candidate "$repo" || return 1
    printf '\n// uncommitted marker\n' >>"$repo/crates/core/src/policy.rs" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    invoke_runner "$repo" "$STANDARD_BASE" 300 120

    expect_eq "$RUN_RC" "1" "runner exit code (dirty worktree)"
    expect_report_eq status "setup_error"
    expect_report_contains reason "worktree_not_clean"
    expect_report_eq exit_code "1"
    expect_report_eq complete "false"
    assert_source_unchanged "$repo"
    return 0
}

##############################################################################
# Malformed-artifact cases ([simulated] shims; artifact edge only)
##############################################################################

case_malformed_common() {
    local variant="$1" repo="$CASE_DIR/repo"
    fixture_standard_candidate "$repo" || return 1
    SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
    printf '[simulated] malformed-artifact case: cargo-mutants shim variant %s\n' "$variant"
    invoke_runner "$repo" "$STANDARD_BASE" 300 120 \
        "PATH=$HARNESS_DIR/shims/$variant:$PATH"

    expect_eq "$RUN_RC" "3" "runner exit code (malformed $variant)"
    expect_report_eq status "inconclusive"
    expect_report_eq reason "artifact_parse_failed"
    expect_report_eq exit_code "3"
    expect_report_eq complete "false"
    expect_report_eq counts.caught "null"
    expect_report_eq counts.missed "null"
    expect_file_contains "$CASE_DIR/report/console.log" "[simulated]"
    assert_source_unchanged "$repo"
    return 0
}

case_malformed_mutants_json() {
    case_malformed_common "malformed-mutants-json"
}

case_malformed_outcomes_json() {
    case_malformed_common "malformed-outcomes-json"
}

case_malformed_outcomes_keys() {
    case_malformed_common "malformed-outcomes-keys"
}

##############################################################################
# Summary
##############################################################################

print_summary() {
    printf '\n=== Q03 contract-test summary ===\n'
    printf 'contract: %s  claim: %s\n' "$CONTRACT_ID" "$CLAIM_TOKEN"
    printf 'runner: %s (git head %s, blob %s)\n' "$RUNNER" "$RUNNER_GIT_HEAD" "$RUNNER_GIT_BLOB"
    printf 'pinned cargo-mutants: %s\n' "$PINNED_TOOL"
    printf 'session: %s\n' "$SESSION_DIR"
    printf 'runtime: %ss\n' "$((SECONDS - START_SECONDS))"
    printf 'case results (per-case behavior assertions, not coverage totals):\n'
    local row group name verdict failed=0
    for row in "${CASE_RESULTS[@]}"; do
        IFS='|' read -r group name verdict <<<"$row"
        printf '  %-14s %-34s %s\n' "$group" "$name" "$verdict"
        if [ "$verdict" = "FAIL" ]; then
            failed=1
        fi
    done
    printf 'group verdicts:\n'
    for group in "${GROUP_ORDER[@]}"; do
        if [ "${GROUP_FAILED[$group]:-0}" -eq 1 ]; then
            printf '  %-14s FAIL\n' "$group"
            failed=1
        else
            printf '  %-14s PASS\n' "$group"
        fi
    done
    if [ "$failed" -eq 0 ]; then
        printf 'overall: PASS (%d cases)\n' "${#CASE_RESULTS[@]}"
        return 0
    fi
    printf 'overall: FAIL (%d cases)\n' "${#CASE_RESULTS[@]}"
    return 1
}

##############################################################################
# Entry point
##############################################################################

main() {
    START_SECONDS=$SECONDS
    unset CARGO_TARGET_DIR 2>/dev/null || true

    parse_args "$@"
    HARNESS_SELF="$HARNESS_DIR/run-contract-tests.sh"

    preflight_host_tools
    validate_runner
    validate_work_dir
    preflight_pinned_tool
    publish_identity

    SESSION_DIR="$WORK_DIR/q03-contract-session-$(date -u +%Y%m%dT%H%M%SZ)-$$"
    if ! mkdir -p "$SESSION_DIR/cases"; then
        setup_error "work_dir_unusable: cannot create $SESSION_DIR"
    fi
    printf 'session: %s\n' "$SESSION_DIR"

    run_case entry-help entry case_entry_help
    run_case entry-required-args entry case_entry_required_args
    run_case entry-runner-rejects entry case_entry_runner_rejects
    run_case entry-workdir-rejects entry case_entry_workdir_rejects

    run_case real-caught real-caught case_real_caught
    run_case real-missed real-missed case_real_missed
    run_case baseline-failure baseline case_baseline_failure
    run_case deadline-budget deadline case_deadline_budget
    run_case signal-cancel signal case_signal_cancel

    run_case empty-diff empty case_empty_diff
    run_case out-of-pilot-diff out-of-pilot case_out_of_pilot_diff
    run_case invalid-base invalid-base case_invalid_base
    run_case wrong-pin wrong-pin case_wrong_pin
    run_case odd-filenames-rename-delete odd-names case_odd_names_rename_delete
    run_case dirty-worktree dirty case_dirty_worktree

    run_case malformed-mutants-json malformed case_malformed_mutants_json
    run_case malformed-outcomes-json malformed case_malformed_outcomes_json
    run_case malformed-outcomes-keys malformed case_malformed_outcomes_keys

    if print_summary; then
        exit 0
    fi
    exit 2
}

# Resolve the harness location with pure shell (before python3 preflight) so
# usage/setup failures work on a minimal host, then load the libraries and go.
case "$0" in
    */*) HARNESS_DIR="$(cd "$(dirname "$0")" && pwd -P)" ;;
    *) HARNESS_DIR="$(pwd -P)" ;;
esac
# shellcheck source=lib.sh
. "$HARNESS_DIR/lib.sh"
# shellcheck source=fixtures.sh
. "$HARNESS_DIR/fixtures.sh"

main "$@"
