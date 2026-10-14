#!/usr/bin/env python3
"""Offline comparisons and reviewed dataset exports; never calls a provider."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import shutil

SCHEMA = 1
IGNORED = {".git", "target", "node_modules", "__pycache__"}
SECRET = re.compile(
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----|\b(?:sk-[A-Za-z0-9_-]{16,}|"
    r"gh[pousr]_[A-Za-z0-9]{20,}|AKIA[A-Z0-9]{16})\b|"
    r"(?i:(?:api[_-]?key|password|secret|access[_-]?token)\s*[:=]\s*[\"']?[^\s\"',}]{8,})"
)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def ratio(numerator, denominator):
    return numerator / denominator if denominator else None


def percentile(values, fraction):
    values = sorted(values)
    return values[max(0, math.ceil(len(values) * fraction) - 1)] if values else None


def verified(result):
    return (result.get("completed") is True and result.get("tests_passing_after") is True
            and result.get("files_correctly_modified", 0) >= 1)


def summarize(report):
    results = report["individual_results"]
    telemetry = [r.get("telemetry") for r in results]
    measured = [t for t in telemetry if t is not None]
    caches = [t["cache"] for t in measured]
    calls = sum(c["calls"] for c in caches)
    successes = sum(verified(r) for r in results)
    missing_usage = sum(t["missing_usage_calls"] for t in measured)
    missing_cost = sum(t["missing_cost_calls"] for t in measured)
    missing_runs = sum(t is None or t["cache"]["calls"] == 0 for t in telemetry)
    complete_cost = bool(results) and not missing_runs and not missing_cost
    known_cost = sum(t["known_estimated_cost_usd"] for t in measured)
    times = [r["time_to_complete_ms"] for r in results]
    first = [v for t in measured for v in t.get("first_content_ms", [])]
    return {
        "runs": len(results), "verified_successes": successes,
        "verified_success_rate": ratio(successes, len(results)),
        "provider_attempts": calls, "missing_telemetry_runs": missing_runs,
        "missing_usage_calls": missing_usage, "missing_cost_calls": missing_cost,
        "failed_attempts": sum(t["failed_attempts"] for t in measured),
        "interrupted_attempts": sum(t["interrupted_attempts"] for t in measured),
        "token_hit_rate": ratio(sum(c["cache_read_tokens"] for c in caches),
                                sum(c["measured_input_tokens"] for c in caches)),
        "request_hit_rate": ratio(sum(c["calls_with_hits"] for c in caches),
                                  sum(c["cache_reporting_calls"] - c["invalid_cache_calls"] for c in caches)),
        "measurement_coverage": ratio(sum(c["cache_reporting_calls"] for c in caches), calls),
        "cache_write_known_subtotal": sum(c.get("cache_write_tokens") or 0 for c in caches),
        "cache_write_reporting_runs": sum(c.get("cache_write_tokens") is not None for c in caches),
        "known_estimated_cost_usd": known_cost,
        "complete_estimated_cost_usd": known_cost if complete_cost else None,
        "cost_per_verified_success_usd": ratio(known_cost, successes) if complete_cost else None,
        "reported_token_subtotal": sum(t["reported_input_tokens"] + t["reported_output_tokens"] for t in measured),
        "tokens_complete": bool(results) and not missing_runs and not missing_usage,
        "runtime_median_ms": percentile(times, .5), "runtime_p95_ms": percentile(times, .95),
        "first_content_median_ms": percentile(first, .5), "first_content_p95_ms": percentile(first, .95),
        "interventions": sum(r["interventions"] for r in results),
    }


def compare(baseline, candidate):
    a, b = summarize(baseline), summarize(candidate)
    def tasks(report):
        return sorted((r["task_id"], r["task_description"]) for r in report["individual_results"])
    same_tasks = tasks(baseline) == tasks(candidate) and bool(a["runs"])
    ma, mb = baseline.get("metadata") or {}, candidate.get("metadata") or {}
    same_runtime = all(ma.get(k) is not None and ma.get(k) == mb.get(k)
                       for k in ("runtime_mode", "policy_mode"))
    same_price = all(ma.get(k) is not None and ma.get(k) == mb.get(k)
                     for k in ("pricing_fingerprint", "cost_source"))
    comparable = same_tasks and same_runtime
    cost_comparable = comparable and same_price and all(
        x["complete_estimated_cost_usd"] is not None for x in (a, b))
    token_comparable = comparable and a["tokens_complete"] and b["tokens_complete"]
    deltas = {}
    for key in ("verified_success_rate", "runtime_median_ms", "runtime_p95_ms",
                "cost_per_verified_success_usd", "reported_token_subtotal"):
        eligible = comparable
        if key.startswith("cost_"):
            eligible = cost_comparable
        if key.startswith("reported_token"):
            eligible = token_comparable
        deltas[key] = b[key] - a[key] if eligible and a[key] is not None and b[key] is not None else None
    return {"schema_version": SCHEMA, "baseline": a, "candidate": b,
            "same_task_definitions": same_tasks, "same_runtime_and_policy": same_runtime,
            "costs_comparable": cost_comparable, "tokens_comparable": token_comparable,
            "candidate_minus_baseline": deltas,
            "limitations": ["Task IDs/descriptions do not prove equal starting snapshots; check persisted run manifests.",
                            "Preparation labels do not prove cold/warm provider state.",
                            "No causal benefit is established without repeated controlled runs."]}


def project_split(project_id):
    # Entire project lineage stays together, including related tasks/sessions.
    return "held_out" if int(digest(project_id.encode())[:8], 16) % 5 == 0 else "train"


def export_cases(suite, output, project_id):
    suite = suite.resolve()
    cases = []
    pending_files = []
    for task_path in sorted(suite.glob("*/task.json")):
        if task_path.is_symlink() or task_path.parent.is_symlink():
            raise ValueError("symlinked task definitions are unsupported")
        task = json.loads(task_path.read_text())
        snapshot = (task_path.parent / task["project_snapshot_path"]).resolve()
        if not snapshot.is_relative_to(suite) or not snapshot.is_dir():
            raise ValueError("snapshot must be a directory within the supplied suite")
        if not task.get("expected_outcomes"):
            raise ValueError(f"{task_path}: expected outcomes are required for a curated case")
        for outcome in task["expected_outcomes"]:
            for name in outcome["files_changed"]:
                p = Path(name)
                if p.is_absolute() or ".." in p.parts:
                    raise ValueError("grader paths must stay within the snapshot")
        files = []
        task_id = task["id"]
        if not re.fullmatch(r"[A-Za-z0-9_-]+", task_id):
            raise ValueError("unsafe task ID")
        for file in sorted(snapshot.rglob("*")):
            rel = file.relative_to(snapshot)
            if any(part in IGNORED for part in rel.parts):
                continue
            if file.is_symlink():
                raise ValueError(f"symlink rejected: {rel}")
            if not file.is_file() or file == task_path:
                continue
            if file.name.startswith(".env") or file.suffix in {".pem", ".key", ".p12"}:
                raise ValueError(f"credential file rejected: {rel}")
            data = file.read_bytes()
            text = data.decode("utf-8")
            if SECRET.search(text):
                raise ValueError(f"possible credential in {rel}; remove before export")
            files.append({"path": rel.as_posix(), "sha256": digest(data)})
            pending_files.append((file, Path(task_id) / "snapshot" / rel))
        if SECRET.search(task["description"]):
            raise ValueError("possible credential in task description")
        cases.append({"schema_version": SCHEMA, "kind": "benchmark_case", "task_id": task_id,
                      "project_id": project_id, "split": project_split(project_id),
                      "definition_sha256": digest(task_path.read_bytes()),
                      "description": task["description"], "expected_outcomes": task["expected_outcomes"],
                      "grader_version": "concerto-eval-patterns-v1",
                      "environment": "See snapshot Cargo.toml/Cargo.lock; run with the repository toolchain.",
                      "snapshot": files, "snapshot_path": f"{task_id}/snapshot",
                      "curation": "requires_local_review"})
    if not cases:
        raise ValueError("no curated cases found")
    output.mkdir(parents=True, exist_ok=False)
    for source, relative in pending_files:
        dest = output / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, dest)
    write_json(output / "manifest.json", {"schema_version": SCHEMA, "cases": cases})


def curate_examples(rows):
    accepted, rejected, seen = [], [], set()
    for number, row in enumerate(rows, 1):
        required = ("project_id", "task_id", "session_id", "visible_input", "visible_output", "verification")
        if (row.get("approved_for_export") is not True
                or not all(isinstance(row.get(k), str) and row[k].strip() for k in required)
                or row.get("externally_verified") is not True):
            rejected.append({"row": number, "reason": "missing approval, provenance, content or external verification"})
            continue
        # Allowlist only visible content; discard arbitrary fields/reasoning.
        result = {key: row[key] for key in required}
        changed = False
        for key in ("visible_input", "visible_output", "verification"):
            clean = SECRET.sub("[REDACTED]", result[key])
            changed |= clean != result[key]
            result[key] = clean
        identity = digest(json.dumps({k: result[k] for k in ("project_id", "task_id", "visible_input", "visible_output")}, sort_keys=True).encode())
        if identity in seen:
            continue
        seen.add(identity)
        result.update(schema_version=SCHEMA, kind="candidate_training_example",
                      split=project_split(row["project_id"]), example_id=identity,
                      curation="requires_local_review", redaction_applied=changed)
        accepted.append(result)
    return {"schema_version": SCHEMA, "examples": accepted, "rejected": rejected,
            "redaction": "best_effort_pattern_scan; human_review_required", "uploaded": False}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    comparison = commands.add_parser("compare")
    comparison.add_argument("baseline", type=Path)
    comparison.add_argument("candidate", type=Path)
    comparison.add_argument("--output", type=Path, required=True)
    cases = commands.add_parser("export-cases")
    cases.add_argument("suite", type=Path)
    cases.add_argument("--project-id", required=True)
    cases.add_argument("--output", type=Path, required=True)
    examples = commands.add_parser("export-examples")
    examples.add_argument("input", type=Path, help="Explicitly reviewed JSONL, never raw audit summaries")
    examples.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.output.exists():
            raise ValueError("output already exists; choose a new path")
        if args.command == "compare":
            write_json(args.output, compare(json.loads(args.baseline.read_text()), json.loads(args.candidate.read_text())))
        elif args.command == "export-cases":
            export_cases(args.suite, args.output, args.project_id)
        else:
            write_json(args.output, curate_examples(json.loads(line) for line in args.input.read_text().splitlines() if line.strip()))
    except (ValueError, OSError, KeyError, TypeError) as error:
        parser.exit(2, f"error: {error}\n")


if __name__ == "__main__":
    main()
