#!/usr/bin/env python3
"""Black-box Q01 v1 acceptance checks; never edits the supplied Q02 runner."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
CONTRACT = "CX/20261005-q01-v1/001"
PIN = "27.1.0"
PILOT = (
    "crates/core/src/policy.rs",
    "crates/core/src/shell_security.rs",
    "crates/shell/src/parser.rs",
)
ODD = "crates/core/src/space [glob]\n$(literal) café.rs"
COUNT_KEYS = ("generated", "caught", "missed", "timeout", "unviable")
STATUSES = {
    "passed", "skipped_no_rust_changes", "skipped_out_of_pilot",
    "setup_error", "missed", "inconclusive", "baseline_failed",
}


class CheckFailed(Exception):
    pass


def require(condition, message):
    if not condition:
        raise CheckFailed(message)


def run(argv, cwd, env=None):
    result = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, check=False)
    if result.returncode:
        raise CheckFailed(f"{argv[0]} exited {result.returncode}: "
                          f"{result.stderr.decode(errors='replace')[-2000:]}")
    return result.stdout


def clean_env():
    env = os.environ.copy()
    for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"):
        env.pop(name, None)
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
               GIT_TERMINAL_PROMPT="0", CARGO_NET_OFFLINE="true",
               CARGO_TERM_COLOR="never", PYTHONDONTWRITEBYTECODE="1")
    return env


def git(root, *args):
    return run(["git", "-C", str(root), *args], root, clean_env())


def write(root, name, contents):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents, encoding="utf-8")


def commit(root, message):
    git(root, "add", "--all")
    git(root, "-c", "core.hooksPath=/dev/null", "commit", "-qm", message)
    return git(root, "rev-parse", "HEAD").decode().strip()


def fixture(root, variant="pilot", template="caught"):
    """Return explicit base/head/merge-base and expected path selection."""
    root.mkdir(parents=True)
    git(root, "init", "-q")
    git(root, "config", "user.name", "Q03 fixture")
    git(root, "config", "user.email", "fixture@example.invalid")
    write(root, "Cargo.toml", '[workspace]\nmembers = ["crates/core", "crates/shell"]\nresolver = "2"\n')
    write(root, "Cargo.lock", 'version = 4\n\n[[package]]\nname = "concerto-core"\nversion = "0.0.0"\n\n[[package]]\nname = "concerto-shell"\nversion = "0.0.0"\n')
    write(root, ".gitignore", "/target/\n")
    for name in ("core", "shell"):
        write(root, f"crates/{name}/Cargo.toml",
              f'[package]\nname = "concerto-{name}"\nversion = "0.0.0"\nedition = "2021"\n')
    write(root, "crates/core/src/lib.rs", "pub mod policy;\npub mod shell_security;\n")
    write(root, "crates/shell/src/lib.rs", "pub mod parser;\n")
    source = (HERE / "fixtures/caught.rs").read_text()
    for path in PILOT:
        write(root, path, source)
    if variant in {"deadline", "cancel_int", "cancel_term"}:
        write(root, "crates/core/build.rs", '''fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(path) = std::env::var("Q03_CHILD_READY") {
        std::fs::write(path, std::process::id().to_string())?;
    }
    std::thread::sleep(std::time::Duration::from_secs(120));
    Ok(())
}
''')
    base = commit(root, "fixture baseline")
    merge_base = base
    selected, uncovered = [PILOT[0]], []
    if variant == "empty":
        write(root, "README.md", "Documentation-only candidate.\n")
        selected = []
    elif variant == "outside":
        write(root, ODD, "pub fn outside() -> bool { true }\n")
        selected, uncovered = [], [ODD]
    elif variant == "deleted":
        (root / PILOT[0]).unlink()
        write(root, "crates/core/src/lib.rs", "pub mod shell_security;\n")
        selected, uncovered = [], [PILOT[0], "crates/core/src/lib.rs"]
    elif variant == "renamed":
        new = "crates/core/src/renamed policy.rs"
        (root / PILOT[0]).rename(root / new)
        write(root, "crates/core/src/lib.rs", '#[path = "renamed policy.rs"]\npub mod policy;\npub mod shell_security;\n')
        selected, uncovered = [], [new, "crates/core/src/lib.rs"]
    else:
        if variant == "diverged":
            git(root, "checkout", "-qb", "other-base")
            write(root, "crates/core/src/base_only.rs", "// absent from the candidate\n")
            base = commit(root, "change only on base branch")
            git(root, "checkout", "-q", "--detach", merge_base)
        if variant == "multi":
            selected = list(PILOT)
        for path in selected:
            text = (HERE / f"fixtures/{template}.rs").read_text()
            write(root, path, text + "\n// Candidate selects this whole file.\n")
        if variant == "mixed":
            write(root, ODD, "// explicitly uncovered\n")
            uncovered = [ODD]
        if variant == "config":
            write(root, ".cargo/mutants.toml", 'exclude_globs = ["**"]\n')
    head = commit(root, "fixture candidate")
    if variant == "dirty":
        with (root / PILOT[0]).open("a") as handle:
            handle.write("// Uncommitted change must not be tested as HEAD.\n")
    if variant == "untracked":
        write(root, "untracked.rs", "// untracked input\n")
    return dict(base=base, head=head, merge_base=merge_base,
                selected=selected, uncovered=uncovered)


def snapshot(root):
    """Include ignored inputs too; only Git internals and Cargo output are omitted."""
    result = {}
    for path in root.rglob("*"):
        relative = path.relative_to(root)
        if relative.parts[0] in {".git", "target"}:
            continue
        if path.is_symlink():
            result[str(relative)] = ["symlink", os.readlink(path)]
        elif path.is_file():
            result[str(relative)] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def read_json(path):
    def invalid_constant(value):
        raise ValueError(f"non-JSON constant {value}")
    try:
        return json.loads(path.read_text(), parse_constant=invalid_constant)
    except (OSError, ValueError) as error:
        raise CheckFailed(f"invalid/missing JSON {path}: {error}") from error


def verify_report(report_dir, exit_code, expected_code, status, state,
                  check_paths=True, expected_raw=None):
    require(exit_code == expected_code, f"exit {exit_code}, expected {expected_code}")
    report = read_json(report_dir / "gate-report.json")
    require(isinstance(report, dict), "report must be an object")
    keys = {"schema_version", "contract_id", "tool_version", "base_sha", "head_sha",
            "merge_base_sha", "selected_files", "uncovered_files", "skipped_files",
            "coverage_scope", "status", "reason", "exit_code", "tool_exit_codes",
            "counts", "complete", "duration_seconds", "budget_seconds", "artifact_paths"}
    require(keys <= report.keys(), f"missing report fields: {sorted(keys - report.keys())}")
    require(type(report["schema_version"]) is int and report["schema_version"] == 1,
            "schema_version must be integer 1")
    require(report["contract_id"] == CONTRACT, "wrong contract ID")
    require(report["coverage_scope"] == "pilot_only", "overstated coverage scope")
    require(report["status"] in STATUSES and report["status"] == status,
            f"status {report['status']!r}, expected {status!r}")
    require(type(report["exit_code"]) is int and report["exit_code"] == exit_code,
            "report exit disagrees with process")
    require(isinstance(report["reason"], str) and report["reason"].strip(), "empty reason")
    require(report["tool_version"] is None or isinstance(report["tool_version"], str),
            "invalid tool_version type")
    for key in ("base_sha", "head_sha", "merge_base_sha"):
        value = report[key]
        require(value is None or (isinstance(value, str) and len(value) == 40
                                 and all(c in "0123456789abcdef" for c in value)),
                f"invalid {key}")
    for key in ("selected_files", "uncovered_files", "skipped_files", "artifact_paths"):
        value = report[key]
        require(isinstance(value, list) and all(isinstance(p, str) for p in value),
                f"{key} must be a string array")
        require(len(value) == len(set(value)), f"duplicate {key}")
        for name in value:
            require(name and not Path(name).is_absolute() and ".." not in Path(name).parts,
                    f"non-relative/escaping path in {key}: {name!r}")
    require(set(report["selected_files"]) <= set(PILOT), "mutation selection escaped pilot")
    require(not (set(report["selected_files"]) & set(report["uncovered_files"])),
            "a path is both selected and uncovered")
    require(isinstance(report["tool_exit_codes"], list)
            and all(type(x) is int for x in report["tool_exit_codes"]), "bad raw exit codes")
    if expected_raw is not None:
        require(expected_raw in report["tool_exit_codes"], "raw tool exit was discarded")
    for key in ("duration_seconds", "budget_seconds"):
        value = report[key]
        require(type(value) in (int, float) and math.isfinite(value) and value >= 0,
                f"invalid {key}")
    counts = report["counts"]
    require(isinstance(counts, dict) and set(COUNT_KEYS) <= counts.keys(), "missing counts")
    for key in COUNT_KEYS:
        require(counts[key] is None or (type(counts[key]) is int and counts[key] >= 0),
                f"invalid count {key}")
    require(type(report["complete"]) is bool, "complete must be boolean")
    if check_paths:
        for key, expected in (("base_sha", state["base"]), ("head_sha", state["head"]),
                              ("merge_base_sha", state["merge_base"] )):
            require(report[key] == expected, f"wrong {key}")
        require(set(report["selected_files"]) == set(state["selected"]), "wrong selected files")
        require(set(state["uncovered"]) <= set(report["uncovered_files"]),
                "uncovered Rust paths lost (including special characters/deletions)")
    if status == "passed":
        require(report["complete"] and report["selected_files"], "empty/incomplete false pass")
        require(report["tool_version"] == PIN, "pass with unpinned tool")
        require(all(type(counts[k]) is int for k in COUNT_KEYS), "complete pass has unknown counts")
        require(counts["caught"] > 0 and counts["missed"] == counts["timeout"] == 0,
                "pass hides missed/timeout/zero viable mutants")
        require(counts["generated"] == sum(counts[k] for k in COUNT_KEYS[1:]),
                "complete counts do not reconcile")
        verify_pass_artifacts(report_dir, report["selected_files"])
    if status == "missed":
        require(type(counts["missed"]) is int and counts["missed"] > 0, "missed without evidence")
    if status.startswith("skipped_"):
        require(not report["selected_files"], "skip silently dropped eligible files")
    if status in {"inconclusive", "baseline_failed", "setup_error"}:
        require(not report["complete"], "failure falsely marked complete")
    for name in report["artifact_paths"]:
        path = report_dir / name
        require(path.resolve().is_relative_to(report_dir.resolve()), "artifact symlink escape")
        require(path.exists(), f"listed artifact absent: {name}")
    require((report_dir / "console.log").is_file(), "console.log missing")
    return report


def verify_pass_artifacts(report_dir, selected):
    """Require real artifact attribution per file, not merely a global caught count."""
    caught = set()
    for path in report_dir.rglob("outcomes.json"):
        lab = read_json(path)
        require(lab.get("cargo_mutants_version") == PIN, "raw outcome version mismatch")
        outcomes = lab.get("outcomes", [])
        baseline_ok = any(o.get("scenario") == "Baseline" and o.get("summary") == "Success"
                          for o in outcomes)
        for outcome in outcomes:
            scenario = outcome.get("scenario")
            if isinstance(scenario, dict) and "Mutant" in scenario:
                require(baseline_ok, "passed mutant without successful baseline evidence")
                file = scenario["Mutant"].get("file")
                require(file in selected, "raw outcomes include an out-of-scope mutant")
                require(outcome.get("summary") in {"CaughtMutant", "Unviable"},
                        "raw outcomes contradict pass")
                if outcome["summary"] == "CaughtMutant":
                    caught.add(file)
    require(set(selected) <= caught, "no caught viable mutant evidence for each selected file")


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    try:
        # Some sandboxes expose /proc from another PID namespace. Its absence
        # or unrelated zombie must not make a live child appear cleaned up.
        proc_self = int(Path("/proc/self/stat").read_text().split()[0])
        if proc_self == os.getpid():
            stat = Path(f"/proc/{pid}/stat").read_text()
            return stat.rsplit(")", 1)[1].split()[0] != "Z"
    except (OSError, ValueError, IndexError):
        pass
    return True


def stop_group(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=5)


def tool_ready(env):
    if not shutil.which("cargo", path=env["PATH"]):
        return "Cargo is not installed"
    try:
        result = subprocess.run(["cargo", "mutants", "--version"], env=env,
                                capture_output=True, text=True, timeout=15)
    except (OSError, subprocess.TimeoutExpired) as error:
        return str(error)
    if result.returncode or result.stdout.strip() != f"cargo-mutants {PIN}":
        return f"real cargo-mutants {PIN} is required; observed {result.stdout.strip()!r}"
    return None


def case_matrix():
    # (name, mode, fixture variant, expected exit, status, raw tool exit)
    cases = [
        ("real_caught", "real", "pilot", 0, "passed", 0),
        ("real_missed", "real", "weak", 2, "missed", 2),
        ("real_baseline", "real", "broken", 4, "baseline_failed", 4),
        ("real_deadline", "real", "deadline", 3, "inconclusive", None),
        ("real_sigint", "real", "cancel_int", 130, "inconclusive", None),
        ("real_sigterm", "real", "cancel_term", 143, "inconclusive", None),
    ]
    for variant, status in (("empty", "skipped_no_rust_changes"),
                            ("outside", "skipped_out_of_pilot"),
                            ("deleted", "skipped_out_of_pilot"),
                            ("renamed", "skipped_out_of_pilot")):
        cases.append((f"scope_{variant}", "caught", variant, 0, status, None))
    for variant in ("multi", "mixed", "diverged", "subdir", "config"):
        cases.append((f"sim_{variant}", "caught", variant, 0, "passed", 0))
    for mode, code, status, raw in (
        ("missed", 2, "missed", 2), ("timeout", 3, "inconclusive", 3),
        ("build_timeout", 3, "inconclusive", 3),
        ("baseline_timeout", 4, "baseline_failed", 4),
        ("mixed_timeout", 3, "inconclusive", 3),
        ("unviable", 3, "inconclusive", 0), ("zero", 3, "inconclusive", 0),
        ("malformed", 1, "setup_error", 0), ("missing", 1, "setup_error", 0),
        ("internal", 1, "setup_error", 70), ("invalid_diff", 1, "setup_error", 6),
        ("mismatch", 1, "setup_error", None),
    ):
        cases.append((f"sim_{mode}", mode, "pilot", code, status, raw))
    for variant in ("dirty", "untracked", "invalid_base", "non_git", "missing_base", "missing_tool",
                    "same_dirs", "inside_source", "reused_report", "symlink_source"):
        cases.append((f"guard_{variant}", "caught", variant, 1, "setup_error", None))
    return cases


def execute_case(case, runner, work, env, real_blocker, deadline=None):
    name, mode, variant, code, status, raw = case
    entry = {"case": name, "evidence": "real" if mode == "real" else "simulated_tool",
             "expected_exit": code, "expected_status": status}
    if mode == "real" and real_blocker:
        return dict(entry, result="blocked", detail=real_blocker)
    if deadline is not None and time.monotonic() >= deadline:
        return dict(entry, result="blocked", detail="harness 720-second budget exhausted")
    area = work / name
    area.mkdir()
    root, report_dir, scratch = area / "repo", area / "report", area / "scratch"
    template = {"weak": "missed", "broken": "baseline_failed"}.get(variant, "caught")
    state = fixture(root, variant, template)
    before = snapshot(root)
    before_status = git(root, "status", "--porcelain=v1", "-z")
    child_marker, trace = area / "child.pid", area / "tool-calls.jsonl"
    env = env.copy()
    env["Q03_CHILD_READY"] = str(child_marker)
    if mode != "real":
        fake_bin = area / "bin"
        fake_bin.mkdir()
        shim = HERE / "fake_cargo.py"
        for command in ("cargo", "cargo-mutants"):
            (fake_bin / command).symlink_to(shim)
        env.update(PATH=str(fake_bin) + os.pathsep + env["PATH"], Q03_SCENARIO=mode,
                   Q03_TRACE=str(trace), Q03_EXPECTED_FILES=json.dumps(state["selected"]))
        if variant == "missing_tool":
            env["Q03_SCENARIO"] = "missing_tool"
    cwd = root / "crates/core" if variant == "subdir" else root
    if variant == "non_git":
        cwd = area / "not-a-repository"
        cwd.mkdir()
    base = "refs/heads/does-not-exist" if variant == "invalid_base" else state["base"]
    budget = 10 if mode != "real" or variant == "deadline" else 120
    report_arg, scratch_arg = str(report_dir), str(scratch)
    if variant == "subdir":
        report_arg, scratch_arg = os.path.relpath(report_dir, cwd), os.path.relpath(scratch, cwd)
    if variant == "same_dirs":
        scratch_arg = report_arg
    if variant == "inside_source":
        report_arg = str(root / "forbidden-report")
    if variant == "reused_report":
        write(report_dir, "prior-evidence.txt", "preserve this earlier evidence\n")
    if variant == "symlink_source":
        (area / "source-link").symlink_to(root, target_is_directory=True)
        scratch_arg = str(area / "source-link")
    argv = ["bash", str(runner), "--report-dir", report_arg, "--scratch-dir", scratch_arg,
            "--budget-seconds", str(budget)]
    if variant != "missing_base":
        argv += ["--base", base]
    entry.update(command=argv, cwd=str(cwd), fixture=state)
    process = None
    try:
        with (area / "runner.log").open("wb") as log:
            process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT,
                                       start_new_session=True)
            if variant in {"cancel_int", "cancel_term"}:
                ready_until = min(time.monotonic() + 60, deadline or float("inf"))
                while not child_marker.exists() and process.poll() is None and time.monotonic() < ready_until:
                    time.sleep(0.05)
                require(child_marker.exists(), "real Cargo child did not reach readiness marker")
                os.kill(process.pid, signal.SIGINT if variant == "cancel_int" else signal.SIGTERM)
            try:
                remaining = min(budget + 35, max(0.1, (deadline or float("inf")) - time.monotonic()))
                observed = process.wait(timeout=remaining)
            except subprocess.TimeoutExpired as error:
                raise CheckFailed("runner exceeded work budget plus cleanup allowance") from error
        entry["observed_exit"] = observed
        require(snapshot(root) == before, "runner modified candidate source or created input files")
        require(git(root, "status", "--porcelain=v1", "-z") == before_status,
                "candidate Git status changed")
        require(git(root, "rev-parse", "HEAD").decode().strip() == state["head"],
                "runner changed the candidate HEAD")
        if variant in {"same_dirs", "inside_source", "reused_report", "symlink_source"}:
            require(observed == 1, f"unsafe output/scratch path exited {observed}, expected 1")
            if variant == "reused_report":
                require((report_dir / "prior-evidence.txt").read_text() == "preserve this earlier evidence\n",
                        "pre-existing evidence was overwritten")
            return dict(entry, result="passed", report_check="unsafe-path rejection; no report required")
        if variant in {"deadline", "cancel_int", "cancel_term"}:
            require(child_marker.exists(), "real process path was not exercised")
            pid = int(child_marker.read_text())
            until = time.monotonic() + 3
            while alive(pid) and time.monotonic() < until:
                time.sleep(0.05)
            require(not alive(pid), "Cargo child remains alive after runner termination")
        report = verify_report(report_dir, observed, code, status, state,
                               check_paths=not variant.startswith(("dirty", "untracked", "invalid_", "non_", "missing_")),
                               expected_raw=raw)
        entry["gate_report"] = str(report_dir / "gate-report.json")
        entry["counts"] = report["counts"]
        entry["result"] = "passed"
    except (CheckFailed, OSError, ValueError) as error:
        entry.update(result="failed", detail=str(error))
    finally:
        if process is not None:
            stop_group(process)
        if child_marker.exists():
            # The marker is written by our own Rust build script. Record a leak
            # above before rescuing it, including children in a separate group.
            try:
                pid = int(child_marker.read_text())
                if alive(pid):
                    os.kill(pid, signal.SIGKILL)
            except (OSError, ValueError):
                pass
    return entry


def prepare_work(path):
    path = path.expanduser().resolve()
    require(not path.is_relative_to(Path("/tmp").resolve()), "work directory cannot be under /tmp")
    require(not path.exists(), "work directory must be fresh; previous evidence is never deleted")
    source_root = HERE.parents[1]
    require(not path.is_relative_to(source_root), "work directory must be outside the source checkout")
    parent = path.parent
    require(parent.is_dir(), "work directory parent must exist")
    kind = run(["stat", "-f", "-c", "%T", str(parent)], parent).decode().strip()
    require(kind not in {"tmpfs", "ramfs"}, "work directory must be disk-backed")
    path.mkdir()
    return path


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runner", required=True, type=Path)
    parser.add_argument("--work-dir", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        require(sys.platform.startswith("linux"), "Q01 v1 fixture pilot currently supports Linux")
        runner = args.runner.expanduser().resolve()
        require(runner.is_file(), "--runner must identify an accessible Q02 script")
        runner_repo = subprocess.run(["git", "-C", str(runner.parent), "rev-parse", "--show-toplevel"],
                                     capture_output=True, text=True, env=clean_env())
        if runner_repo.returncode == 0:
            require(not args.work_dir.expanduser().resolve().is_relative_to(Path(runner_repo.stdout.strip())),
                    "work directory must be outside the runner checkout")
        work = prepare_work(args.work_dir)
        digest = hashlib.sha256(runner.read_bytes()).hexdigest()
        env = clean_env()
        blocker = tool_ready(env)
        checks = []
        deadline = time.monotonic() + 720
        for case in case_matrix():
            try:
                result = execute_case(case, runner, work, env, blocker, deadline)
            except (CheckFailed, OSError) as error:
                result = {"case": case[0], "result": "failed", "detail": str(error)}
            checks.append(result)
            print(f"{result['result'].upper()}: {case[0]} {result.get('detail', '')}", flush=True)
        unchanged = hashlib.sha256(runner.read_bytes()).hexdigest() == digest
        failed = any(c["result"] == "failed" for c in checks) or not unchanged
        blocked = any(c["result"] == "blocked" for c in checks)
        summary = {"contract_id": CONTRACT, "cargo_mutants_pin": PIN, "runner": str(runner),
                   "runner_sha256": digest, "runner_unchanged": unchanged,
                   "result": "failed" if failed else "blocked" if blocked else "passed", "cases": checks}
        (work / "contract-results.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(f"Evidence: {work / 'contract-results.json'}", flush=True)
        return 1 if failed else 2 if blocked else 0
    except (CheckFailed, OSError) as error:
        print(f"BLOCKED: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
