#!/usr/bin/env python3
"""Explicitly simulated cargo-mutants 27.1.0 edge cases, never mutation evidence.

Artifact shape follows the pinned upstream src/{mutant,outcome,scenario,process}.rs:
https://github.com/sourcefrog/cargo-mutants/tree/v27.1.0/src
Only the Q03 harness prepends this executable to PATH. It is never installed.
"""
import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess
import sys

PIN = "27.1.0"
FILES = ["crates/core/src/policy.rs", "crates/core/src/shell_security.rs",
         "crates/shell/src/parser.rs"]


def values(args, long, short=None):
    found = []
    for index, value in enumerate(args):
        if value in {long, short} and index + 1 < len(args):
            found.append(args[index + 1])
        elif value.startswith(long + "="):
            found.append(value.split("=", 1)[1])
        elif short and value.startswith(short) and len(value) > len(short):
            found.append(value[len(short):])
    return found


def mutant(file, index):
    span = {"start": {"line": 2, "column": 5}, "end": {"line": 2, "column": 15}}
    replacement = "true" if index % 2 == 0 else "false"
    return {"name": f"{file}:2:5: replace allowed -> bool with {replacement}",
            "package": "concerto-shell" if "/shell/" in file else "concerto-core",
            "file": file, "function": {"function_name": "allowed", "return_type": "-> bool",
                                         "span": span},
            "span": span, "replacement": replacement, "genre": "FnValue"}


def phase(name, status):
    return {"phase": name, "duration": 0.01, "process_status": status,
            "argv": ["cargo", "test" if name == "Test" else "build"]}


def outcome(scenario, summary, index=0):
    log = "logs/baseline.log" if scenario == "Baseline" else f"logs/mutant-{index}.log"
    phases = [phase("Build", "Success"), phase("Test", "Success")]
    if summary == "CaughtMutant":
        phases[-1] = phase("Test", {"Failure": 101})
    elif summary == "Unviable":
        phases = [phase("Build", {"Failure": 101})]
    elif summary == "Failure":
        phases[-1] = phase("Test", {"Failure": 101})
    elif summary == "Timeout":
        phases[-1] = phase("Test", "Timeout")
    return {"scenario": scenario, "summary": summary, "log_path": log,
            "diff_path": None if scenario == "Baseline" else f"diff/mutant-{index}.diff",
            "phase_results": phases}


def emit(root, mutants, mode):
    root.mkdir(parents=True, exist_ok=True)
    (root / "logs").mkdir(exist_ok=True)
    (root / "diff").mkdir(exist_ok=True)
    (root / "mutants.json").write_text(json.dumps(mutants))
    if mode == "missing":
        return 0
    if mode == "malformed":
        (root / "outcomes.json").write_text('{"outcomes": [')
        return 0
    baseline = outcome("Baseline", "Timeout" if mode == "baseline_timeout" else "Success")
    outcomes = [baseline]
    if mode != "baseline_timeout":
        for index, item in enumerate(mutants):
            summary = {"missed": "MissedMutant", "timeout": "Timeout",
                       "build_timeout": "Timeout", "unviable": "Unviable"}.get(mode, "CaughtMutant")
            if mode == "mixed_timeout":
                summary = "MissedMutant" if index == 0 else "Timeout"
            entry = outcome({"Mutant": item}, summary, index)
            if mode == "build_timeout":
                entry["phase_results"] = [phase("Build", "Timeout")]
            outcomes.append(entry)
    counts = {"caught": 0, "missed": 0, "timeout": 0, "unviable": 0}
    mapping = {"CaughtMutant": "caught", "MissedMutant": "missed",
               "Timeout": "timeout", "Unviable": "unviable"}
    for entry in outcomes:
        (root / entry["log_path"]).write_text("SIMULATED cargo-mutants edge-case output\n")
        if entry["scenario"] != "Baseline":
            counts[mapping[entry["summary"]]] += 1
            (root / entry["diff_path"]).write_text("SIMULATED mutation diff\n")
    lab = {"outcomes": outcomes, "total_mutants": sum(counts.values()), **counts,
           "success": 0, "start_time": "2026-10-05T00:00:00Z",
           "end_time": "2026-10-05T00:00:01Z", "cargo_mutants_version": PIN}
    (root / "outcomes.json").write_text(json.dumps(lab))
    for key, summary in (("caught", "CaughtMutant"), ("missed", "MissedMutant"),
                         ("timeout", "Timeout"), ("unviable", "Unviable")):
        names = [o["scenario"]["Mutant"]["name"] for o in outcomes
                 if isinstance(o["scenario"], dict) and o["summary"] == summary]
        (root / f"{key}.txt").write_text("\n".join(names))
    if mode == "baseline_timeout":
        return 4
    return 3 if counts["timeout"] else 2 if counts["missed"] else 0


def metadata(root):
    packages = []
    for short in ("core", "shell"):
        directory = root / f"crates/{short}"
        name = f"concerto-{short}"
        packages.append({"name": name, "version": "0.0.0", "id": f"path+file://{directory}#{name}@0.0.0",
                         "source": None, "dependencies": [], "features": {}, "edition": "2021",
                         "manifest_path": str(directory / "Cargo.toml"),
                         "targets": [{"kind": ["lib"], "crate_types": ["lib"],
                                      "name": name.replace("-", "_"), "edition": "2021",
                                      "src_path": str(directory / "src/lib.rs"),
                                      "doctest": True, "test": True, "doc": True}]})
    ids = [p["id"] for p in packages]
    return {"packages": packages, "workspace_members": ids, "workspace_default_members": ids,
            "resolve": {"nodes": [{"id": i, "dependencies": [], "deps": [], "features": []}
                                  for i in ids], "root": None}, "version": 1,
            "workspace_root": str(root), "target_directory": str(root / "target"), "metadata": None}


def main():
    args = sys.argv[1:]
    mode = os.environ["Q03_SCENARIO"]
    with Path(os.environ["Q03_TRACE"]).open("a") as log:
        log.write(json.dumps({"argv": args, "mode": mode, "simulated": True}) + "\n")
    if args and args[0] == "mutants":
        args = args[1:]
    elif Path(sys.argv[0]).name == "cargo":
        if "--version" in args or "-V" in args:
            print("cargo 1.96.0 (Q03 SIMULATED metadata driver)")
            return 0
        root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
        if args and args[0] == "metadata":
            print(json.dumps(metadata(root)))
            return 0
        raise ValueError(f"unsupported simulated cargo command: {args!r}")
    if mode == "missing_tool":
        print("SIMULATED: cargo-mutants unavailable", file=sys.stderr)
        return 127
    if "--version" in args or "-V" in args:
        print("cargo-mutants " + ("0.0.0" if mode == "mismatch" else PIN))
        return 0
    filters = values(args, "--file", "-f")
    packages = values(args, "--package", "-p")
    regexes = values(args, "--examine-re")
    root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    files = [f for f in FILES if (root / f).is_file()
             and (not filters or any(fnmatch.fnmatchcase(f, pattern) for pattern in filters))
             and (not packages or ("concerto-shell" if "/shell/" in f else "concerto-core") in packages)
             and (not regexes or any(re.search(pattern, f) for pattern in regexes))]
    expected = set(json.loads(os.environ["Q03_EXPECTED_FILES"]))
    if not set(files) <= expected or not files:
        raise ValueError(f"selection does not restrict mutations to changed pilot files: {files!r}")
    if "--in-place" in args or values(args, "--baseline") == ["skip"]:
        raise ValueError("in-place mutation or skipped baseline violates Q01")
    config = values(args, "--config")
    if "--no-config" not in args and not (config and not Path(config[-1]).read_text().strip()):
        raise ValueError("hidden config is not excluded (--no-config or an empty explicit config)")
    jobs = values(args, "--jobs", "-j") or [os.environ.get("CARGO_MUTANTS_JOBS", "")]
    if jobs != ["1"] or "--shuffle" in args:
        raise ValueError("expected one mutation worker and deterministic ordering")
    if list(map(float, values(args, "--timeout"))) != [300.0] or list(map(float, values(args, "--build-timeout"))) != [600.0]:
        raise ValueError("phase timeout caps differ from Q01 v1")
    mutants = [mutant(f, i) for i, f in enumerate(files)]
    if mode == "zero":
        mutants = []
    if mode == "mixed_timeout":
        mutants = [mutant(files[0], 0), mutant(files[0], 1)]
    if "--list" in args:
        print(json.dumps(mutants))
        return 0
    if mode in {"internal", "invalid_diff"}:
        return 70 if mode == "internal" else 6
    outputs = values(args, "--output", "-o")
    directory = Path(outputs[-1] if outputs else os.environ.get("CARGO_MUTANTS_OUTPUT", "."))
    return emit(directory / "mutants.out", mutants, mode)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, KeyError, OSError) as error:
        print(f"Q03 SIMULATION ERROR: {error}", file=sys.stderr)
        sys.exit(70)
