#!/usr/bin/env bash
# Q03 harness library — logging, path checks, case machinery, assertions and
# runner invocation. Sourced by run-contract-tests.sh (set -euo pipefail);
# assertion helpers always return 0 and record failures in CASE_FAILURES.

##############################################################################
# Logging / setup failures
##############################################################################

log() {
    printf '%s\n' "$*"
}

# Every harness-level validation failure is a setup_error and exits 1,
# mirroring the Q02 runner's failure class.
setup_error() {
    printf 'setup_error: %s\n' "$*" >&2
    exit 1
}

# Absolute, symlink-resolved path (works for paths that do not exist yet).
absp() {
    python3 -c 'import os, sys; print(os.path.realpath(os.path.abspath(sys.argv[1])))' "$1"
}

# True when $1 is $2 or lives inside $2.
path_within() {
    local p="${1%/}" d="${2%/}"
    if [ "$p" = "$d" ]; then
        return 0
    fi
    case "$p" in
        "$d"/*) return 0 ;;
    esac
    return 1
}

# Filesystem type of an existing path; empty when unknown.
fs_type() {
    stat -f -c %T -- "$1" 2>/dev/null || true
}

##############################################################################
# Case machinery (populated by run_case in run-contract-tests.sh)
##############################################################################

CASE_FAILURES=()
CASE_RESULTS=() # rows: group|name|PASS|FAIL
GROUP_ORDER=()
declare -A GROUP_FAILED=()

case_fail() {
    CASE_FAILURES+=("$1")
    printf 'assert-fail: %s\n' "$1"
}

# expect_eq <actual> <expected> <label>
expect_eq() {
    if [ "$1" != "$2" ]; then
        case_fail "$3: expected [$2], got [$1]"
    fi
}

# expect_contains <haystack> <needle> <label>
expect_contains() {
    case "$1" in
        *"$2"*) ;;
        *) case_fail "$3: [$2] not found in [$1]" ;;
    esac
}

# expect_ge <actual> <min> <label> (actual must be a non-negative integer)
expect_ge() {
    case "$1" in
        '' | *[!0-9]*)
            case_fail "$3: expected an integer >= $2, got [$1]"
            return 0
            ;;
    esac
    if [ "$1" -lt "$2" ]; then
        case_fail "$3: expected >= $2, got $1"
    fi
    return 0
}

# expect_file_contains <file> <needle> — byte-level check on a produced file.
expect_file_contains() {
    if [ ! -f "$1" ]; then
        case_fail "file missing: $1"
        return 0
    fi
    if ! grep -qF -- "$2" "$1"; then
        case_fail "$1: [$2] not found"
    fi
    return 0
}

##############################################################################
# gate-report.json assertions (REPORT_FILE is set per case by run_case)
##############################################################################

# report_value <key> → stdout value; returns report.py's rc (0/3/4/...).
report_value() {
    python3 "$HARNESS_DIR/report.py" get "$REPORT_FILE" "$1" 2>/dev/null
}

expect_report_eq() {
    local key="$1" expected="$2" value rc=0
    value="$(report_value "$key")" || rc=$?
    case "$rc" in
        0)
            if [ "$value" != "$expected" ]; then
                case_fail "report.$key: expected [$expected], got [$value]"
            fi
            ;;
        3) case_fail "report.$key: gate-report.json missing or unreadable" ;;
        4) case_fail "report.$key: key missing from gate-report.json" ;;
        *) case_fail "report.$key: report helper failed (rc=$rc)" ;;
    esac
    return 0
}

expect_report_contains() {
    local key="$1" needle="$2" rc=0
    python3 "$HARNESS_DIR/report.py" contains "$REPORT_FILE" "$key" "$needle" >/dev/null 2>&1 || rc=$?
    case "$rc" in
        0) ;;
        1)
            local got
            got="$(report_value "$key" || true)"
            case_fail "report.$key: [$needle] not found in [$got]"
            ;;
        3) case_fail "report.$key: gate-report.json missing or unreadable" ;;
        4) case_fail "report.$key: key missing from gate-report.json" ;;
        *) case_fail "report.$key: report helper failed (rc=$rc)" ;;
    esac
    return 0
}

expect_report_ge() {
    local key="$1" min="$2" value rc=0
    value="$(report_value "$key")" || rc=$?
    case "$rc" in
        0) expect_ge "$value" "$min" "report.$key" ;;
        3) case_fail "report.$key: gate-report.json missing or unreadable" ;;
        4) case_fail "report.$key: key missing from gate-report.json" ;;
        *) case_fail "report.$key: report helper failed (rc=$rc)" ;;
    esac
    return 0
}

##############################################################################
# Runner invocation
##############################################################################

# Print the observed report digest (published into the case log).
publish_report_summary() {
    if [ -f "$REPORT_FILE" ]; then
        python3 "$HARNESS_DIR/report.py" summary "$REPORT_FILE" ||
            printf 'report: (summary failed)\n'
    else
        printf 'report: (missing at %s)\n' "$REPORT_FILE"
    fi
}

# invoke_runner <repo> <base-sha> <timeout-seconds> [budget-seconds] [VAR=VAL ...]
#
# Runs the Q02 runner from the fixture cwd with explicit --base/--report-dir/
# --scratch-dir, bounded by the harness-level timeout. Never returns nonzero:
# the observed exit code lands in RUN_RC, output in RUN_OUT.
invoke_runner() {
    local repo="$1" base="$2" tmo="$3"
    shift 3
    local budget=""
    case "${1:-}" in
        '' | *[!0-9]*) budget="" ;;
        *)
            budget="$1"
            shift
            ;;
    esac
    local -a envs=()
    while [ "$#" -gt 0 ]; do
        envs+=("$1")
        shift
    done
    local report="$CASE_DIR/report" scratch="$CASE_DIR/scratch"
    local -a cmd=(
        timeout --kill-after=15 "$tmo"
        env -u CARGO_TARGET_DIR "${envs[@]}"
        bash "$RUNNER"
        --base "$base"
        --report-dir "$report"
        --scratch-dir "$scratch"
    )
    if [ -n "$budget" ]; then
        cmd+=(--budget-seconds "$budget")
    fi

    printf 'fixture-repo: %s\n' "$repo"
    printf 'base-sha: %s\n' "$base"
    printf 'candidate-sha: %s\n' "$(git -C "$repo" rev-parse HEAD)"
    printf 'cmd: cd %q &&' "$repo"
    printf ' %q' "${cmd[@]}"
    printf '\n'

    RUN_OUT="$CASE_DIR/runner.out"
    RUN_RC=0
    (cd "$repo" && "${cmd[@]}") >"$RUN_OUT" 2>&1 || RUN_RC=$?
    printf 'runner-exit: %s\n' "$RUN_RC"
    publish_report_summary
    return 0
}

# invoke_runner_signal <repo> <base-sha> [SIG]
#
# Starts the runner in the background and delivers SIG (default TERM) as soon
# as the tool run has started (run-001/started appears). Returns 1 without a
# signal when the run never started; otherwise returns 0 with RUN_RC set.
invoke_runner_signal() {
    local repo="$1" base="$2" sig="${3:-TERM}"
    local report="$CASE_DIR/report" scratch="$CASE_DIR/scratch"
    local -a cmd=(
        env -u CARGO_TARGET_DIR bash "$RUNNER"
        --base "$base"
        --report-dir "$report"
        --scratch-dir "$scratch"
    )
    printf 'fixture-repo: %s\n' "$repo"
    printf 'base-sha: %s\n' "$base"
    printf 'candidate-sha: %s\n' "$(git -C "$repo" rev-parse HEAD)"
    printf 'cmd (background): cd %q &&' "$repo"
    printf ' %q' "${cmd[@]}"
    printf '\n'

    RUN_OUT="$CASE_DIR/runner.out"
    (cd "$repo" && exec "${cmd[@]}") >"$RUN_OUT" 2>&1 &
    local pid=$!
    local i=0 started=0
    while [ "$i" -lt 600 ]; do # 600 x 0.2s = 120s window
        if [ -f "$report/.gate-state/run-001/started" ]; then
            started=1
            break
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            break
        fi
        sleep 0.2
        i=$((i + 1))
    done
    if [ "$started" -ne 1 ]; then
        RUN_RC=0
        wait "$pid" 2>/dev/null || RUN_RC=$?
        printf 'signal: tool run never started (runner exit=%s before SIG%s)\n' "$RUN_RC" "$sig"
        return 1
    fi
    printf 'signal: delivering SIG%s to runner pid %s while the tool run is active\n' "$sig" "$pid"
    kill -s "$sig" "$pid" 2>/dev/null || true
    i=0
    while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 450 ]; do # 90s finalize bound
        sleep 0.2
        i=$((i + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        printf 'signal: runner still alive 90s after SIG%s; sending SIGKILL\n' "$sig"
        kill -s KILL "$pid" 2>/dev/null || true
    fi
    RUN_RC=0
    wait "$pid" 2>/dev/null || RUN_RC=$?
    printf 'runner-exit: %s\n' "$RUN_RC"
    publish_report_summary
    return 0
}

##############################################################################
# Source-unchanged proof and harness self-spawn
##############################################################################

# Call after the candidate commit, before invoking the runner:
#   SOURCE_SNAPSHOT="$(snapshot_source_state "$repo")"
# then assert_source_unchanged "$repo" after the invocation.
assert_source_unchanged() {
    local repo="$1" now
    now="$(snapshot_source_state "$repo")"
    if [ "$now" = "$SOURCE_SNAPSHOT" ]; then
        printf 'source-unchanged: yes\n'
    else
        printf 'source-unchanged: NO\n--- before ---\n%s\n--- after ---\n%s\n' \
            "$SOURCE_SNAPSHOT" "$now"
        case_fail "candidate source changed after the runner invocation"
    fi
    return 0
}

# spawn_harness [args...] — invoke this harness again for entry-contract
# checks. Output: $CASE_DIR/spawn.out, code: SPAWN_RC.
# NOTE: entry cases must never pass both a valid --runner and a valid
# --work-dir, or the child would start its own full run.
spawn_harness() {
    SPAWN_RC=0
    SPAWN_OUT="$CASE_DIR/spawn.out"
    bash "$HARNESS_SELF" "$@" >"$SPAWN_OUT" 2>&1 || SPAWN_RC=$?
    printf 'spawn: bash %q' "$HARNESS_SELF"
    if [ "$#" -gt 0 ]; then
        printf ' %q' "$@"
    fi
    printf ' -> exit %s\n' "$SPAWN_RC"
    return 0
}

spawn_output() {
    cat "$SPAWN_OUT" 2>/dev/null || true
}
