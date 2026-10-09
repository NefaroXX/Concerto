#!/usr/bin/env bash
#
# Q02 — executable mutation gate (diff scoped).
#
# Contract: CX/20261005-q01-v1/001
#
# Runs cargo-mutants against the pilot file set that the current change
# touches and writes a machine-readable verdict to <report-dir>/gate-report.json.
#
# Exit codes:
#    0  passed, skipped_no_rust_changes, skipped_out_of_pilot
#    1  setup_error (usage, worktree, refs, tool, diff, internal)
#    2  missed (one or more mutants survived the test suite)
#    3  inconclusive (budget, tool timeout, unparseable artifacts, no viable mutants)
#    4  baseline failed
#  130  interrupted (SIGINT)
#  143  terminated (SIGTERM)
#
# Usage:
#   bash scripts/mutation-gate.sh --base <REF> --report-dir <DIR> \
#       --scratch-dir <DIR> [--budget-seconds <1..1800>]
set -euo pipefail

readonly CONTRACT_ID="CX/20261005-q01-v1/001"
readonly SCHEMA_VERSION="1"
readonly COVERAGE_SCOPE="pilot_only"
readonly REPORT_FILE="gate-report.json"
readonly STATE_DIR=".gate-state"
readonly MUTANTS_PIN="27.1.0"
readonly MUTANTS_INSTALL_HINT="cargo install cargo-mutants --version 27.1.0 --locked"
readonly DEFAULT_BUDGET=1800
readonly MIN_BUDGET=1
readonly MAX_BUDGET=1800
readonly KILL_GRACE_SECONDS=30
# Cargo-mutants per-run timeouts (seconds).
readonly MUTANTS_BUILD_TIMEOUT=600
readonly MUTANTS_TEST_TIMEOUT=300

# Fixed pilot file set chosen by Q01; paths are repo-relative.
readonly -a PILOT_FILES=(
    "crates/core/src/policy.rs"
    "crates/core/src/shell_security.rs"
    "crates/shell/src/parser.rs"
)

# --- CLI / run state (single-shot globals) ---
BASE_REF=""
REPORT_DIR=""
SCRATCH_DIR=""
BUDGET_SECONDS="$DEFAULT_BUDGET"
GATE_START=0
ROOT=""
REPORT_READY=0
GATE_DIR=""

BASE_SHA=""
HEAD_SHA=""
MERGE_BASE_SHA=""
TOOL_VERSION=""
PROBE_SEEN=""
declare -a MUTANTS_CMD=()

RUST_CHANGED=0
RUNS_PLANNED=0
declare -a PKGS=()
declare -A PKG_OF=()

SELECTED_FILES=()
UNCOVERED_FILES=()
SKIPPED_FILES=()

SETUP_REASON=""
CANCEL=""
CANCEL_EXIT=0
BUDGET_EXCEEDED=0
DEADLINE=0
CHILD_PID=""
CHILD_PGID=""
CHILD_KILLED=0
RUN_RC=0
FINALIZED=0
FINALIZE_ATTEMPTED=0
EXIT_CODE=0
STATUS=""
REASON=""
COMPLETE=""

##############################################################################
# Logging and usage
##############################################################################

log() {
    printf 'mutation-gate: %s\n' "$*"
}

log_err() {
    printf 'mutation-gate: %s\n' "$*" >&2
}

usage() {
    cat <<'EOF'
Q02 mutation gate — diff-scoped cargo-mutants run.

Usage:
  bash scripts/mutation-gate.sh --base <REF> --report-dir <DIR> --scratch-dir <DIR> [--budget-seconds <N>]
  bash scripts/mutation-gate.sh --help

Required:
  --base <REF>          Base git ref to diff against; must resolve to a commit.
  --report-dir <DIR>    Fresh (empty) directory that receives gate-report.json.
  --scratch-dir <DIR>   Fresh (empty) directory for tool logs and build trees.

Optional:
  --budget-seconds <N>  Wall-clock budget for the mutation runs, 1..1800 (default 1800).
  --help                Show this help and exit 0.

Both directories must be distinct, not nested in each other, outside the
repository checkout, and not under /tmp or /dev/shm.

Report: <report-dir>/gate-report.json
Exit codes: 0 passed/skipped, 1 setup_error, 2 missed, 3 inconclusive,
            4 baseline_failed, 130 SIGINT, 143 SIGTERM.
EOF
}

usage_error() {
    log_err "usage_error: $1"
    log_err "run 'bash scripts/mutation-gate.sh --help' for usage"
    exit 1
}

# Fail before the report directory exists: nothing can be written, so only
# stderr carries the reason.
hard_fail() {
    log_err "setup_error: $1"
    exit 1
}

# Sanitize a value for state files: single line, no control whitespace.
san() {
    printf '%s' "$1" | tr '\n\r\t' '   '
}

##############################################################################
# Argument parsing
##############################################################################

parse_args() {
    local opt val
    while [ "$#" -gt 0 ]; do
        opt="$1"
        case "$opt" in
            --help | -h)
                usage
                exit 0
                ;;
            --base | --report-dir | --scratch-dir | --budget-seconds)
                if [ "$#" -lt 2 ]; then
                    usage_error "missing value for $opt"
                fi
                val="$2"
                case "$val" in
                    --*) usage_error "missing value for $opt" ;;
                esac
                case "$opt" in
                    --base) BASE_REF="$val" ;;
                    --report-dir) REPORT_DIR="$val" ;;
                    --scratch-dir) SCRATCH_DIR="$val" ;;
                    --budget-seconds) BUDGET_SECONDS="$val" ;;
                esac
                shift 2
                ;;
            --base=*)
                BASE_REF="${opt#--base=}"
                [ -n "$BASE_REF" ] || usage_error "--base requires a non-empty value"
                shift
                ;;
            --report-dir=*)
                REPORT_DIR="${opt#--report-dir=}"
                [ -n "$REPORT_DIR" ] || usage_error "--report-dir requires a non-empty value"
                shift
                ;;
            --scratch-dir=*)
                SCRATCH_DIR="${opt#--scratch-dir=}"
                [ -n "$SCRATCH_DIR" ] || usage_error "--scratch-dir requires a non-empty value"
                shift
                ;;
            --budget-seconds=*)
                BUDGET_SECONDS="${opt#--budget-seconds=}"
                shift
                ;;
            --)
                usage_error "positional arguments are not supported"
                ;;
            -*)
                usage_error "unknown option: $opt"
                ;;
            *)
                usage_error "unexpected positional argument: $opt"
                ;;
        esac
    done

    [ -n "$BASE_REF" ] || usage_error "missing required argument: --base <REF>"
    [ -n "$REPORT_DIR" ] || usage_error "missing required argument: --report-dir <DIR>"
    [ -n "$SCRATCH_DIR" ] || usage_error "missing required argument: --scratch-dir <DIR>"
    case "$BUDGET_SECONDS" in
        # ?????* rejects values too large for shell arithmetic (and every value > 1800).
        '' | *[!0-9]* | ?????*)
            usage_error "--budget-seconds must be an integer between $MIN_BUDGET and $MAX_BUDGET"
            ;;
    esac
    if [ "$BUDGET_SECONDS" -lt "$MIN_BUDGET" ] || [ "$BUDGET_SECONDS" -gt "$MAX_BUDGET" ]; then
        usage_error "--budget-seconds must be an integer between $MIN_BUDGET and $MAX_BUDGET"
    fi
}

##############################################################################
# Small helpers
##############################################################################

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

# True when $1 matches one of the fixed pilot files.
is_pilot() {
    local p
    for p in "${PILOT_FILES[@]}"; do
        if [ "$p" = "$1" ]; then
            return 0
        fi
    done
    return 1
}

# append $2 to the array named by $1, unless already present
add_unique() {
    local -n _add_unique_arr="$1"
    local val="$2" existing
    for existing in "${_add_unique_arr[@]}"; do
        if [ "$existing" = "$val" ]; then
            return 0
        fi
    done
    _add_unique_arr+=("$val")
}

# write the array named by $2 to the NUL-delimited file $1
write_nul_list() {
    local out="$1"
    local -n _write_nul_arr="$2"
    local item
    : >"$out"
    for item in "${_write_nul_arr[@]}"; do
        printf '%s\0' "$item" >>"$out"
    done
}

count_nul() {
    local n
    n="$(tr -cd '\0' <"$1" | wc -c)"
    printf '%s' "$((n))"
}

##############################################################################
# Setup: report/scratch directories, git state, tool pin
##############################################################################

prepare_report_dir() {
    case "$REPORT_DIR" in
        /tmp | /tmp/* | /dev/shm | /dev/shm/*)
            hard_fail "report_dir_not_allowed: $REPORT_DIR must not live under /tmp or /dev/shm"
            ;;
    esac
    if [ -e "$REPORT_DIR" ]; then
        if [ ! -d "$REPORT_DIR" ]; then
            hard_fail "report_dir_not_a_directory: $REPORT_DIR"
        fi
        if [ -n "$(ls -A "$REPORT_DIR" 2>/dev/null)" ]; then
            hard_fail "report_dir_not_fresh: $REPORT_DIR already exists and is not empty"
        fi
    fi
    if ! mkdir -p "$REPORT_DIR" 2>/dev/null; then
        hard_fail "report_dir_unusable: cannot create $REPORT_DIR"
    fi
    if [ ! -w "$REPORT_DIR" ]; then
        hard_fail "report_dir_unusable: $REPORT_DIR is not writable"
    fi
    GATE_DIR="$REPORT_DIR/$STATE_DIR"
    if ! mkdir -p "$GATE_DIR" 2>/dev/null; then
        hard_fail "report_dir_unusable: cannot create $GATE_DIR"
    fi
    REPORT_READY=1
}

discover_root() {
    local out
    if ! out="$(git rev-parse --show-toplevel 2>/dev/null)"; then
        setup_fail "git_root_not_found: run this from inside the repository checkout"
        return 1
    fi
    if ! ROOT="$(cd "$out" && pwd -P)"; then
        setup_fail "git_root_not_found: cannot enter $out"
        return 1
    fi
}

# Filesystem type of an existing path; empty when unknown.
fs_type() {
    stat -f -c %T -- "$1" 2>/dev/null || true
}

prepare_scratch_dir() {
    local fst

    if path_within "$REPORT_DIR" "$ROOT" || [ "$REPORT_DIR" = "$ROOT" ]; then
        setup_fail "report_dir_inside_repo: $REPORT_DIR must be outside the checkout"
        return 1
    fi
    case "$SCRATCH_DIR" in
        /tmp | /tmp/* | /dev/shm | /dev/shm/*)
            setup_fail "scratch_dir_not_allowed: $SCRATCH_DIR must not live under /tmp or /dev/shm"
            return 1
            ;;
    esac
    if path_within "$SCRATCH_DIR" "$REPORT_DIR" || path_within "$REPORT_DIR" "$SCRATCH_DIR"; then
        setup_fail "report_and_scratch_overlap: $REPORT_DIR and $SCRATCH_DIR must be distinct and not nested"
        return 1
    fi
    if path_within "$SCRATCH_DIR" "$ROOT" || [ "$SCRATCH_DIR" = "$ROOT" ]; then
        setup_fail "scratch_dir_inside_repo: $SCRATCH_DIR must be outside the checkout"
        return 1
    fi
    if [ -e "$SCRATCH_DIR" ]; then
        if [ ! -d "$SCRATCH_DIR" ]; then
            setup_fail "scratch_dir_not_a_directory: $SCRATCH_DIR"
            return 1
        fi
        if [ -n "$(ls -A "$SCRATCH_DIR" 2>/dev/null)" ]; then
            setup_fail "scratch_dir_not_fresh: $SCRATCH_DIR already exists and is not empty"
            return 1
        fi
    fi
    if ! mkdir -p "$SCRATCH_DIR" 2>/dev/null; then
        setup_fail "scratch_dir_unusable: cannot create $SCRATCH_DIR"
        return 1
    fi

    fst="$(fs_type "$REPORT_DIR")"
    case "$fst" in
        tmpfs | ramfs)
            setup_fail "report_dir_on_tmpfs: $REPORT_DIR is on $fst"
            return 1
            ;;
    esac
    fst="$(fs_type "$SCRATCH_DIR")"
    case "$fst" in
        tmpfs | ramfs)
            setup_fail "scratch_dir_on_tmpfs: $SCRATCH_DIR is on $fst"
            return 1
            ;;
    esac
    if ! mkdir -p "$SCRATCH_DIR/tmp" 2>/dev/null; then
        setup_fail "scratch_dir_unusable: cannot create $SCRATCH_DIR/tmp"
        return 1
    fi
    # Keep every tool build tree on the (disk-backed) scratch directory.
    TMPDIR="$SCRATCH_DIR/tmp"
    export TMPDIR
    # An inherited CARGO_TARGET_DIR would redirect every tool build tree out of
    # this isolation; the contract harness unsets it, production callers may not.
    unset CARGO_TARGET_DIR 2>/dev/null || true
}

# The worktree must match HEAD: file existence checks below rely on it, and a
# dirty tree would otherwise be silently diffed into the gate.
check_clean_tree() {
    local out
    if ! out="$(git status --porcelain --untracked-files=all 2>/dev/null)"; then
        setup_fail "git_status_failed: cannot inspect the worktree"
        return 1
    fi
    if [ -n "$out" ]; then
        setup_fail "worktree_not_clean: $(printf '%s' "$out" | head -n 1) (git status --porcelain --untracked-files=all)"
        return 1
    fi
}

resolve_refs() {
    if ! git rev-parse --verify --quiet "${BASE_REF}^{commit}" >/dev/null 2>&1; then
        setup_fail "base_ref_invalid: '${BASE_REF}' does not resolve to a commit"
        return 1
    fi
    if ! BASE_SHA="$(git rev-parse --verify --quiet "${BASE_REF}^{commit}" 2>/dev/null)"; then
        setup_fail "base_ref_invalid: '${BASE_REF}' does not resolve to a commit"
        return 1
    fi
    if ! HEAD_SHA="$(git rev-parse --verify --quiet 'HEAD^{commit}' 2>/dev/null)"; then
        setup_fail "head_missing: HEAD does not resolve to a commit"
        return 1
    fi
    if ! MERGE_BASE_SHA="$(git merge-base "$BASE_SHA" "$HEAD_SHA" 2>/dev/null)"; then
        setup_fail "merge_base_not_found: '${BASE_REF}' and HEAD have no common ancestor"
        return 1
    fi
    # git may print several lines with --all; keep the first.
    MERGE_BASE_SHA="$(printf '%s' "$MERGE_BASE_SHA" | head -n 1)"
}

# Record a version probe that actually reports cargo-mutants (diagnostics for
# a mismatch); error/help text mentioning cargo-mutants is not a version.
note_probe() {
    local first
    first="$(printf '%s' "$1" | awk 'NF { print $1; exit }')"
    if [ "$first" = "cargo-mutants" ]; then
        PROBE_SEEN="$(printf '%s' "$1" | tr '\n\r\t' '   ')"
    fi
}

# Resolve cargo-mutants and verify the pinned version. Never installs: the
# pin is a CI/setup responsibility (see MUTANTS_INSTALL_HINT).
#
# cargo-mutants' own clap root command is `cargo`, so a directly invoked
# binary only accepts options after the `mutants` subcommand token; `cargo
# mutants ...` supplies that token itself. Both spellings are probed and the
# spelling that reports the pinned version is the one the runs use.
resolve_tool() {
    local probe="" token cand bin
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
        note_probe "$probe"
        token="$(printf '%s' "$probe" | awk 'END { print $NF }')"
        if [ "$token" = "$MUTANTS_PIN" ]; then
            MUTANTS_CMD=("$cand" mutants)
            TOOL_VERSION="$MUTANTS_PIN"
            return 0
        fi
        probe="$("$cand" --version 2>&1)" || true
        note_probe "$probe"
        token="$(printf '%s' "$probe" | awk 'END { print $NF }')"
        if [ "$token" = "$MUTANTS_PIN" ]; then
            MUTANTS_CMD=("$cand")
            TOOL_VERSION="$MUTANTS_PIN"
            return 0
        fi
    done

    if command -v cargo >/dev/null 2>&1; then
        probe="$(cargo mutants --version 2>&1)" || true
        note_probe "$probe"
        token="$(printf '%s' "$probe" | awk 'END { print $NF }')"
        if [ "$token" = "$MUTANTS_PIN" ]; then
            MUTANTS_CMD=(cargo mutants)
            TOOL_VERSION="$MUTANTS_PIN"
            return 0
        fi
    fi

    if [ -n "$PROBE_SEEN" ]; then
        setup_fail "cargo_mutants_version_mismatch: found '${PROBE_SEEN}' but need ${MUTANTS_PIN}; install it with: $MUTANTS_INSTALL_HINT"
        return 1
    fi
    setup_fail "cargo_mutants_not_found: install it with: $MUTANTS_INSTALL_HINT"
    return 1
}

##############################################################################
# Diff classification (NUL-safe)
##############################################################################

# Classify one path touched between merge-base and HEAD:
#   non-.rs          -> skipped_files
#   .rs, not pilot   -> uncovered_files
#   .rs, pilot, live -> selected_files
#   .rs, pilot, gone -> uncovered_files
classify_path() {
    local path="$1"
    case "$path" in
        *.rs) ;;
        *)
            add_unique SKIPPED_FILES "$path"
            return 0
            ;;
    esac
    RUST_CHANGED=1
    if ! is_pilot "$path"; then
        add_unique UNCOVERED_FILES "$path"
        return 0
    fi
    if [ ! -f "$ROOT/$path" ]; then
        add_unique UNCOVERED_FILES "$path"
        return 0
    fi
    add_unique SELECTED_FILES "$path"
    return 0
}

# Fill SELECTED_FILES / UNCOVERED_FILES / SKIPPED_FILES and RUST_CHANGED.
collect_diff() {
    local ns="$GATE_DIR/name-status.bin"
    local err="$GATE_DIR/diff.err"
    local tok old_path head_path

    if ! git diff --name-status -z --find-renames "$MERGE_BASE_SHA" "$HEAD_SHA" >"$ns" 2>"$err"; then
        local why
        why="$(tr '\n\r' '  ' <"$err" | cut -c1-200)"
        setup_fail "diff_failed: ${why:-git diff produced no error output}"
        return 1
    fi

    while IFS= read -r -d '' tok; do
        old_path=""
        head_path=""
        case "$tok" in
            R* | C*)
                # <status>\0<old path>\0<new path>\0 — the new path is at HEAD.
                IFS= read -r -d '' old_path || old_path=""
                IFS= read -r -d '' head_path || head_path=""
                ;;
            *)
                IFS= read -r -d '' head_path || head_path=""
                ;;
        esac
        [ -n "$head_path" ] || continue
        classify_path "$head_path"
    done <"$ns"

    # Persist immediately: map_packages and the report reader consume these.
    write_nul_list "$GATE_DIR/selected.nul" SELECTED_FILES
    write_nul_list "$GATE_DIR/uncovered.nul" UNCOVERED_FILES
    write_nul_list "$GATE_DIR/skipped.nul" SKIPPED_FILES
}

##############################################################################
# Package mapping and run planning
##############################################################################

# Map every selected file to its owning Cargo package (nearest ancestor
# manifest with a [package] name). Writes $GATE_DIR/pkgmap.nul as
# path\0package\0 pairs; any unmapped file is a setup error.
map_packages() {
    local err="$GATE_DIR/map.err"
    if ! python3 - "$GATE_DIR" "$ROOT" >"$err" 2>&1 <<'__PY_MAP__'
import os
import sys
import tomllib
from pathlib import Path

gate = Path(sys.argv[1])
root = sys.argv[2]

try:
    raw = (gate / "selected.nul").read_bytes()
except OSError as exc:
    raise SystemExit(f"selected_list_unreadable: {exc}")
files = [b.decode("utf-8", "surrogateescape") for b in raw.split(b"\0") if b]


def package_of(rel_path: str) -> str | None:
    directory = os.path.realpath(os.path.join(root, os.path.dirname(rel_path)))
    cache: dict[str, str | None] = {}
    while True:
        if directory in cache:
            return cache[directory]
        manifest = os.path.join(directory, "Cargo.toml")
        if os.path.isfile(manifest):
            try:
                with open(manifest, "rb") as handle:
                    data = tomllib.load(handle)
            except Exception as exc:  # noqa: BLE001 - reported as setup_error
                raise SystemExit(f"package_manifest_unreadable: {manifest}: {exc}")
            name = (data.get("package") or {}).get("name")
            if not isinstance(name, str) or not name:
                raise SystemExit(f"package_name_missing: {manifest}")
            return name
        parent = os.path.dirname(directory)
        if parent == directory:
            cache[directory] = None
            return None
        directory = parent


out = bytearray()
missing = []
for rel in files:
    pkg = package_of(rel)
    if pkg is None:
        missing.append(rel)
        continue
    out += rel.encode("utf-8", "surrogateescape") + b"\0" + pkg.encode("utf-8") + b"\0"
if missing:
    raise SystemExit("package_not_found: " + ", ".join(missing))
(gate / "pkgmap.nul").write_bytes(bytes(out))
__PY_MAP__
    then
        local why
        why="$(tr '\n\r' '  ' <"$err" | cut -c1-300)"
        setup_fail "${why:-package_mapping_failed}"
        return 1
    fi
}

# Load path -> package pairs produced by map_packages.
load_pkgmap() {
    PKG_OF=()
    local path pkg
    [ -f "$GATE_DIR/pkgmap.nul" ] || return 0
    while IFS= read -r -d '' path && IFS= read -r -d '' pkg; do
        PKG_OF["$path"]="$pkg"
    done <"$GATE_DIR/pkgmap.nul"
}

# One cargo-mutants run per package, first-seen file order preserved.
build_runs() {
    PKGS=()
    local file pkg
    declare -A pkg_seen=()
    mkdir -p "$GATE_DIR/runsrc"
    for file in "${SELECTED_FILES[@]}"; do
        pkg="${PKG_OF[$file]:-}"
        if [ -z "$pkg" ]; then
            setup_fail "package_not_found: $file"
            return 1
        fi
        if [ -z "${pkg_seen[$pkg]:-}" ]; then
            pkg_seen[$pkg]=1
            PKGS+=("$pkg")
            printf '%s\0' "$file" >"$GATE_DIR/runsrc/$pkg.nul"
        else
            printf '%s\0' "$file" >>"$GATE_DIR/runsrc/$pkg.nul"
        fi
    done
    RUNS_PLANNED="${#PKGS[@]}"
}

##############################################################################
# Child process control
##############################################################################

# Send $1 to the tool's whole process group, then escalate to KILL after
# KILL_GRACE_SECONDS. Never signals this script's own process group.
# Liveness is judged on the process GROUP, not on the leader's PID: the
# setsid'd leader can exit while a descendant still ignores TERM, and a
# parent-only check would skip the SIGKILL escalation and leak the orphan.
terminate_child() {
    local sig="$1" pgid="$CHILD_PGID" waited=0
    [ -n "$pgid" ] || return 0
    kill -s "$sig" -- "-$pgid" 2>/dev/null || kill -s "$sig" "$pgid" 2>/dev/null || true
    while kill -0 -- "-$pgid" 2>/dev/null && [ "$waited" -lt "$KILL_GRACE_SECONDS" ]; do
        sleep 1
        waited=$((waited + 1))
    done
    if kill -0 -- "-$pgid" 2>/dev/null; then
        kill -s KILL -- "-$pgid" 2>/dev/null || kill -s KILL "$pgid" 2>/dev/null || true
    fi
}

# Run "$@" in its own session (process group) with output captured to $1.
# Sets RUN_RC, CHILD_KILLED and BUDGET_EXCEEDED; never runs in a subshell so
# those globals survive.
run_child() {
    local logfile="$1"
    shift
    local pid rc=0

    CHILD_KILLED=0
    CHILD_PID=""
    CHILD_PGID=""
    (
        cd "$ROOT" || exit 111
        exec setsid "$@"
    ) >"$logfile" 2>&1 &
    pid=$!
    CHILD_PID=$pid
    # setsid makes the child a session and process-group leader: PGID == PID.
    CHILD_PGID=$pid

    # Wait while either the leader or its process group is alive: a leader
    # that exits must not end budget monitoring while descendants remain.
    while kill -0 "$pid" 2>/dev/null || kill -0 -- "-$CHILD_PGID" 2>/dev/null; do
        if [ "$BUDGET_EXCEEDED" -eq 0 ] && [ "$SECONDS" -ge "$DEADLINE" ]; then
            BUDGET_EXCEEDED=1
            CHILD_KILLED=1
            log "budget of ${BUDGET_SECONDS}s exceeded; terminating the tool run"
            terminate_child TERM
        fi
        # Sub-second poll: a 1s sleep here pushed every budget kill one full
        # second past the deadline (and the post-kill recheck another second).
        sleep 0.1
    done
    wait "$pid" || rc=$?
    CHILD_PID=""
    CHILD_PGID=""
    RUN_RC=$rc
}

##############################################################################
# Mutation runs
##############################################################################

# One cargo-mutants invocation for a single package.
run_one() {
    local idx="$1" pkg="$2"
    local rundir mdir logfile file_count file
    rundir="$(printf '%s/run-%03d' "$GATE_DIR" "$idx")"
    mkdir -p "$rundir"
    mdir="$REPORT_DIR/artifacts/$pkg"
    logfile="$SCRATCH_DIR/tool-run-$idx.log"
    # cargo-mutants creates the --output directory itself (single level), so
    # only its parent may exist here.
    mkdir -p "$REPORT_DIR/artifacts"

    printf '%s' "$pkg" >"$rundir/pkg"
    cp "$GATE_DIR/runsrc/$pkg.nul" "$rundir/files.nul"
    printf '%s' "$mdir" >"$rundir/mutants_dir"
    printf '1' >"$rundir/started"
    printf '0' >"$rundir/completed"
    printf '0' >"$rundir/killed"

    local -a cmd=("${MUTANTS_CMD[@]}" -p "$pkg")
    while IFS= read -r -d '' file; do
        cmd+=(--file "$file")
    done <"$GATE_DIR/runsrc/$pkg.nul"
    cmd+=(
        --output "$mdir"
        --baseline run
        "--build-timeout=$MUTANTS_BUILD_TIMEOUT"
        "--timeout=$MUTANTS_TEST_TIMEOUT"
        --no-config
    )

    file_count="$(count_nul "$rundir/files.nul")"
    log "run $idx/$RUNS_PLANNED: package=$pkg files=$file_count"

    RUN_RC=0
    run_child "$logfile" "${cmd[@]}"

    printf '%s' "$RUN_RC" >"$rundir/exit_code"
    if [ "$CHILD_KILLED" -eq 1 ]; then
        printf '1' >"$rundir/killed"
        printf '0' >"$rundir/completed"
    else
        printf '0' >"$rundir/killed"
        printf '1' >"$rundir/completed"
    fi

    {
        printf '\n===== mutation-gate run %s: package %s =====\n' "$idx" "$pkg"
        printf 'cwd: %s\n' "$ROOT"
        printf 'argv: %s\n' "${cmd[*]}"
        printf 'exit: %s\n' "$RUN_RC"
        cat "$logfile" 2>/dev/null || true
    } >>"$REPORT_DIR/console.log"

    log "run $idx: exit=$RUN_RC"
    if [ "$RUN_RC" -ne 0 ]; then
        tail -n 20 "$logfile" 2>/dev/null || true
    fi
}

# Run every planned package under one global wall-clock budget.
run_all() {
    local idx=0 pkg
    DEADLINE=$((SECONDS + BUDGET_SECONDS))
    for pkg in "${PKGS[@]}"; do
        if [ "$BUDGET_EXCEEDED" -eq 1 ]; then
            break
        fi
        idx=$((idx + 1))
        run_one "$idx" "$pkg"
    done
}

##############################################################################
# State, report, verdict
##############################################################################

write_state() {
    local duration="$1"
    mkdir -p "$GATE_DIR"
    {
        printf 'CONTRACT_ID=%s\n' "$CONTRACT_ID"
        printf 'SCHEMA_VERSION=%s\n' "$SCHEMA_VERSION"
        printf 'COVERAGE_SCOPE=%s\n' "$COVERAGE_SCOPE"
        printf 'TOOL_VERSION=%s\n' "$(san "$TOOL_VERSION")"
        printf 'BASE_REF=%s\n' "$(san "$BASE_REF")"
        printf 'BASE_SHA=%s\n' "$BASE_SHA"
        printf 'HEAD_SHA=%s\n' "$HEAD_SHA"
        printf 'MERGE_BASE_SHA=%s\n' "$MERGE_BASE_SHA"
        printf 'BUDGET_SECONDS=%s\n' "$BUDGET_SECONDS"
        printf 'DURATION_SECONDS=%s\n' "$duration"
    } >"$GATE_DIR/meta.env"
    {
        printf 'SETUP_REASON=%s\n' "$(san "$SETUP_REASON")"
        printf 'CANCEL=%s\n' "$(san "$CANCEL")"
        printf 'CANCEL_EXIT=%s\n' "$CANCEL_EXIT"
        printf 'BUDGET_EXCEEDED=%s\n' "$BUDGET_EXCEEDED"
        printf 'RUST_CHANGED=%s\n' "$RUST_CHANGED"
        printf 'RUNS_PLANNED=%s\n' "$RUNS_PLANNED"
    } >"$GATE_DIR/flags.env"
    write_nul_list "$GATE_DIR/selected.nul" SELECTED_FILES
    write_nul_list "$GATE_DIR/uncovered.nul" UNCOVERED_FILES
    write_nul_list "$GATE_DIR/skipped.nul" SKIPPED_FILES
}

# Parse tool artifacts, decide the verdict, write gate-report.json and
# decision.env (EXIT_CODE / STATUS / REASON / COMPLETE). Never raises:
# every failure path becomes a setup_error with exit 1 or 70.
finalize() {
    if [ "$FINALIZE_ATTEMPTED" -eq 1 ]; then
        return 0
    fi
    FINALIZE_ATTEMPTED=1
    if [ "$REPORT_READY" -ne 1 ]; then
        log_err "setup_error: report directory unavailable; cannot write $REPORT_FILE"
        if [ "$EXIT_CODE" -eq 0 ]; then
            EXIT_CODE=1
        fi
        return 0
    fi

    write_state "$((SECONDS - GATE_START))"

    if ! python3 - "$REPORT_DIR" "$GATE_DIR" "$CONTRACT_ID" <<'__PY_REPORT__'
import json
import shlex
import sys
from pathlib import Path

report_dir = Path(sys.argv[1])
gate = Path(sys.argv[2])
contract_id = sys.argv[3]

REPORT_NAME = "gate-report.json"
STATE_NAME = ".gate-state"
KEYS = ("generated", "caught", "missed", "timeout", "unviable")
TOOL_SETUP_EXITS = (1, 5, 6, 70)


def env_file(name):
    try:
        text = (gate / name).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return {}
    out = {}
    for line in text.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            out[key] = value
    return out


def nul_list(name):
    try:
        raw = (gate / name).read_bytes()
    except OSError:
        return []
    return [b.decode("utf-8", "surrogateescape") for b in raw.split(b"\0") if b]


def read_json(path):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError, TypeError):
        return None


def artifact_path(mutants_dir, name):
    """Locate a cargo-mutants artifact: <out>/<name> or <out>/mutants.out/<name>."""
    if mutants_dir is None:
        return None
    direct = mutants_dir / name
    if direct.exists():
        return direct
    nested = mutants_dir / "mutants.out" / name
    if nested.exists():
        return nested
    return None


def as_int(value, default=0):
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def field(directory, name, default=""):
    try:
        return (directory / name).read_text(encoding="utf-8", errors="replace").strip()
    except OSError:
        return default


meta = env_file("meta.env")
flags = env_file("flags.env")
selected = nul_list("selected.nul")
uncovered = nul_list("uncovered.nul")
skipped = nul_list("skipped.nul")

setup_reason = flags.get("SETUP_REASON", "")
cancel = flags.get("CANCEL", "")
cancel_exit = as_int(flags.get("CANCEL_EXIT"), 0)
budget_exceeded = flags.get("BUDGET_EXCEEDED", "0") == "1"
rust_changed = flags.get("RUST_CHANGED", "0") == "1"
runs_planned = as_int(flags.get("RUNS_PLANNED"), 0)

# --- per-run artifact parsing -------------------------------------------
runs = []
for directory in sorted(gate.glob("run-*")):
    if not directory.is_dir():
        continue
    started = field(directory, "started") == "1"
    completed = field(directory, "completed") == "1"
    killed = field(directory, "killed") == "1"
    mutants_dir_raw = field(directory, "mutants_dir")
    mutants_dir = Path(mutants_dir_raw) if mutants_dir_raw else None
    exit_raw = field(directory, "exit_code")
    exit_code = as_int(exit_raw, None) if exit_raw != "" else None

    mutants_path = artifact_path(mutants_dir, "mutants.json")
    outcomes_path = artifact_path(mutants_dir, "outcomes.json")
    mutants = read_json(mutants_path)
    outcomes = read_json(outcomes_path)
    outcomes_present = outcomes_path is not None
    generated = len(mutants) if isinstance(mutants, list) else None

    ok = False
    counts = None
    if started and completed:
        if isinstance(outcomes, dict) and all(
            isinstance(outcomes.get(key), int)
            for key in ("caught", "missed", "timeout", "unviable")
        ):
            counts = {key: int(outcomes[key]) for key in ("caught", "missed", "timeout", "unviable")}
            ok = isinstance(mutants, list)
        elif isinstance(mutants, list) and len(mutants) == 0 and not outcomes_present:
            counts = {"caught": 0, "missed": 0, "timeout": 0, "unviable": 0}
            ok = True
    if started and exit_code is None:
        ok = False

    runs.append(
        {
            "started": started,
            "completed": completed,
            "killed": killed,
            "exit_code": exit_code,
            "generated": generated,
            "counts": counts,
            "ok": ok,
            "outcomes": outcomes,
        }
    )

started_runs = [run for run in runs if run["started"]]
all_ok = all(run["ok"] for run in started_runs)
all_completed = all(run["completed"] for run in started_runs) and len(started_runs) == runs_planned

if not started_runs:
    counts = {key: None for key in KEYS}
elif all_ok:
    generated_total = 0
    for run in started_runs:
        generated_total += run["generated"] or 0
    counts = {"generated": generated_total}
    for key in ("caught", "missed", "timeout", "unviable"):
        counts[key] = sum(run["counts"][key] for run in started_runs)
else:
    # One unparseable artifact set poisons the whole aggregate.
    counts = {key: None for key in KEYS}

# A selected file is viable when at least one of its mutants could be built
# and executed (anything but Unviable).
viable_files = set()
for run in started_runs:
    outcomes = run["outcomes"]
    if not isinstance(outcomes, dict):
        continue
    for entry in outcomes.get("outcomes") or []:
        if not isinstance(entry, dict):
            continue
        scenario = entry.get("scenario")
        if not isinstance(scenario, dict):
            continue
        mutant = scenario.get("Mutant")
        if not isinstance(mutant, dict):
            continue
        if entry.get("summary") == "Unviable":
            continue
        path = mutant.get("file")
        if isinstance(path, str):
            viable_files.add(path)

tool_exit_codes = [run["exit_code"] for run in started_runs if run["exit_code"] is not None]
live_exits = [
    run["exit_code"]
    for run in started_runs
    if not run["killed"] and run["exit_code"] is not None
]

# --- verdict, in contract precedence ------------------------------------
if setup_reason:
    status, reason, exit_code_out = "setup_error", setup_reason, 1
elif cancel:
    if cancel_exit:
        code = cancel_exit
    elif cancel.upper().endswith("INT"):
        code = 130
    else:
        code = 143
    status, reason, exit_code_out = "inconclusive", f"external_cancelled ({cancel})", code
elif any(code == 4 for code in live_exits):
    status, reason, exit_code_out = "baseline_failed", "baseline_tests_failed (exit 4)", 4
elif any(code in TOOL_SETUP_EXITS for code in live_exits):
    bad = next(code for code in live_exits if code in TOOL_SETUP_EXITS)
    status, reason, exit_code_out = "setup_error", f"tool_exit_{bad}", 1
elif runs_planned == 0:
    if not rust_changed:
        status = "skipped_no_rust_changes"
        reason = "no_rust_changes_since_merge_base"
    else:
        status = "skipped_out_of_pilot"
        reason = "rust_changes_present_but_none_selected_for_the_pilot"
    exit_code_out = 0
elif budget_exceeded:
    status, reason, exit_code_out = "inconclusive", "budget_exceeded", 3
elif not all_completed:
    status, reason, exit_code_out = "inconclusive", "runs_incomplete", 3
elif not all_ok:
    status, reason, exit_code_out = "inconclusive", "artifact_parse_failed", 3
elif any(code == 3 for code in live_exits):
    status, reason, exit_code_out = "inconclusive", "cargo_mutants_timeout (exit 3)", 3
elif any(code not in (0, 2) for code in live_exits):
    bad = next(code for code in live_exits if code not in (0, 2))
    status, reason, exit_code_out = "inconclusive", f"tool_unexpected_exit_{bad}", 3
elif (counts["timeout"] or 0) > 0:
    status = "inconclusive"
    reason = f"mutant_timeouts ({counts['timeout']} timeout)"
    exit_code_out = 3
elif any(path not in viable_files for path in selected):
    missing = next(path for path in selected if path not in viable_files)
    status, reason, exit_code_out = "inconclusive", f"no_viable_mutants: {missing}", 3
elif (counts["missed"] or 0) > 0 or 2 in live_exits:
    status = "missed"
    reason = f"missed_mutants ({counts['missed']} missed)"
    exit_code_out = 2
else:
    status, reason, exit_code_out = "passed", "all_mutants_caught", 0

# `complete` marks a final, fully-parsed verdict: setup errors, cancellations
# and budget cuts are never complete, terminal verdicts always are, and a
# remaining inconclusive verdict is complete only if every run finished and
# every artifact parsed.
if status == "setup_error" or cancel or budget_exceeded:
    complete = False
elif status in (
    "passed",
    "missed",
    "baseline_failed",
    "skipped_no_rust_changes",
    "skipped_out_of_pilot",
):
    complete = True
else:
    complete = all_completed and all_ok


def optional_sha(key):
    value = meta.get(key, "")
    return value if value else None


artifact_paths = []
for path in sorted(report_dir.rglob("*")):
    parts = path.relative_to(report_dir).parts
    if not parts or parts[0] == STATE_NAME:
        continue
    if len(parts) == 1 and parts[0] == REPORT_NAME:
        continue
    if path.is_file():
        artifact_paths.append(path.relative_to(report_dir).as_posix())

tool_version = meta.get("TOOL_VERSION", "") or None

doc = {
    "schema_version": as_int(meta.get("SCHEMA_VERSION"), 1),
    "contract_id": meta.get("CONTRACT_ID", contract_id) or contract_id,
    "tool_version": tool_version,
    "base_sha": optional_sha("BASE_SHA"),
    "head_sha": optional_sha("HEAD_SHA"),
    "merge_base_sha": optional_sha("MERGE_BASE_SHA"),
    "selected_files": selected,
    "uncovered_files": uncovered,
    "skipped_files": skipped,
    "coverage_scope": meta.get("COVERAGE_SCOPE", "pilot_only") or "pilot_only",
    "status": status,
    "reason": reason,
    "exit_code": exit_code_out,
    "tool_exit_codes": tool_exit_codes,
    "counts": {key: counts[key] for key in KEYS},
    "complete": complete,
    "duration_seconds": as_int(meta.get("DURATION_SECONDS"), 0),
    "budget_seconds": as_int(meta.get("BUDGET_SECONDS"), 0),
    "artifact_paths": artifact_paths,
}

with (report_dir / REPORT_NAME).open("w", encoding="utf-8") as handle:
    json.dump(doc, handle, indent=2, ensure_ascii=False)
    handle.write("\n")

(gate / "decision.env").write_text(
    "EXIT_CODE=%d\nSTATUS=%s\nREASON=%s\nCOMPLETE=%d\n"
    % (exit_code_out, shlex.quote(status), shlex.quote(reason), 1 if complete else 0),
    encoding="utf-8",
)
__PY_REPORT__
    then
        log_err "internal_error: could not parse tool artifacts or write $REPORT_FILE"
        STATUS="setup_error"
        REASON="internal_error: report generation failed"
        COMPLETE="0"
        EXIT_CODE=70
        return 0
    fi

    if [ ! -f "$GATE_DIR/decision.env" ]; then
        log_err "internal_error: decision.env was not produced"
        STATUS="setup_error"
        REASON="internal_error: decision.env was not produced"
        COMPLETE="0"
        EXIT_CODE=70
        return 0
    fi
    # shellcheck disable=SC1091
    . "$GATE_DIR/decision.env"
    FINALIZED=1
    return 0
}

# Record a setup failure: write the report when the report directory is usable.
setup_fail() {
    local reason="$1"
    log_err "setup_error: $reason"
    if [ "$REPORT_READY" -ne 1 ]; then
        exit 1
    fi
    SETUP_REASON="$reason"
    FINALIZE_ATTEMPTED=0
    FINALIZED=0
    finalize
    log_summary
    exit "$EXIT_CODE"
}

log_summary() {
    log "status=${STATUS:-unknown} reason=${REASON:-unknown} complete=${COMPLETE:-unknown} exit=${EXIT_CODE}"
    if [ "$REPORT_READY" -eq 1 ]; then
        log "report=$REPORT_DIR/$REPORT_FILE"
    fi
}

finish() {
    finalize
    log_summary
    exit "$EXIT_CODE"
}

##############################################################################
# Signals and unexpected exits
##############################################################################

# SIGINT/SIGTERM: stop the tool, then report an inconclusive gate.
on_signal() {
    local sig="$1" code="$2"
    trap - INT TERM
    set +e
    CANCEL="SIG${sig}"
    CANCEL_EXIT="$code"
    log_err "interrupted by SIG${sig}; terminating the tool run"
    if [ -n "$CHILD_PID" ]; then
        terminate_child TERM
    fi
    if [ "$REPORT_READY" -eq 1 ]; then
        FINALIZE_ATTEMPTED=0
        FINALIZED=0
        finalize
        log_summary
    else
        log_err "setup_error: interrupted before the report directory was ready"
    fi
    exit "$code"
}

# Last-resort safety net: anything that exits without a verdict gets one.
on_exit() {
    local rc=$?
    trap - EXIT INT TERM
    set +e
    if [ "$FINALIZED" -eq 1 ] || [ "$FINALIZE_ATTEMPTED" -eq 1 ]; then
        return 0
    fi
    if [ "$REPORT_READY" -ne 1 ]; then
        # Nothing can be written; preserve the original exit status.
        return 0
    fi
    SETUP_REASON="internal_error (unexpected exit status ${rc})"
    finalize
    log_summary
    exit "$EXIT_CODE"
}

##############################################################################
# Setup helpers that must exist before main runs
##############################################################################

require_host_tools() {
    if ! command -v git >/dev/null 2>&1; then
        hard_fail "git_not_found: git is required"
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        hard_fail "python3_not_found: python3 is required to parse artifacts and write the report"
    fi
    if ! command -v setsid >/dev/null 2>&1; then
        hard_fail "setsid_not_found: setsid is required so the tool run can be terminated as a group"
    fi
}

# Resolve both directories against the caller's working directory before any
# chdir happens.
resolve_paths() {
    local resolved_report resolved_scratch
    if ! resolved_report="$(absp "$REPORT_DIR")"; then
        hard_fail "report_dir_invalid: cannot resolve $REPORT_DIR"
    fi
    if ! resolved_scratch="$(absp "$SCRATCH_DIR")"; then
        hard_fail "scratch_dir_invalid: cannot resolve $SCRATCH_DIR"
    fi
    REPORT_DIR="$resolved_report"
    SCRATCH_DIR="$resolved_scratch"
}

##############################################################################
# Entry point
##############################################################################

main() {
    GATE_START=$SECONDS
    trap 'on_signal INT 130' INT
    trap 'on_signal TERM 143' TERM
    trap on_exit EXIT

    parse_args "$@"
    require_host_tools
    resolve_paths
    prepare_report_dir
    discover_root
    prepare_scratch_dir
    check_clean_tree
    resolve_refs
    resolve_tool

    collect_diff
    if [ "$RUST_CHANGED" -eq 0 ]; then
        log "no .rs changes since $MERGE_BASE_SHA; skipping"
        finish
    fi
    if [ "${#SELECTED_FILES[@]}" -eq 0 ]; then
        log "rust changes present but none in the pilot file set; skipping"
        finish
    fi

    map_packages
    load_pkgmap
    build_runs
    run_all
    finish
}

main "$@"
