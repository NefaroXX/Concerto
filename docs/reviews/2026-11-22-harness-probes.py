"""Run real specialist-loop regressions against the pinned round-two candidate.

Temporarily adds test-only module declarations; restores the original sources
in finally. Run only while the candidate worktree and Cargo target are idle.
No production code is modified. Expected result: one pass, three failures.
"""

from pathlib import Path
import hashlib
import json
import os
import subprocess
import sys


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "/home/sol/Projects/Concerto-worktrees/h40")
    expected = "90afd73207472c9da9c65d67d81b813ab664d4bd"
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    if head != expected:
        raise SystemExit(f"Expected {expected}, found {head}")
    status = subprocess.check_output(["git", "status", "--porcelain"], cwd=root, text=True)
    if status:
        raise SystemExit("Candidate worktree must be clean before the probe")
    fixtures = [Path(__file__).with_suffix(".rs").resolve(),
                Path(__file__).with_name("2026-11-22-harness-runner-probes.rs").resolve()]
    sources = [root / "crates/orchestrator/src/agents/generic.rs",
               root / "crates/orchestrator/src/agent_runner.rs"]
    marker = b"#[cfg(test)]\nmod tests {\n"
    edits = []
    for source, fixture in zip(sources, fixtures):
        original = source.read_bytes()
        if original.count(marker) != 1:
            raise SystemExit(f"Expected one test-module declaration in {source}")
        declaration = f"    #[path = {json.dumps(str(fixture))}]\n    mod review_round_two_probes;\n"
        patched = original.replace(marker, marker + declaration.encode(), 1)
        edits.append((source, original, patched))
    target = Path(__file__).resolve().parents[2] / "target"
    out = target / "harness-review-2026-11-22"
    out.mkdir(parents=True, exist_ok=True)
    command = ["cargo", "test", "-j1", "-p", "concerto-orchestrator", "--lib",
               "review_round_two_probes", "--", "--nocapture"]
    manifest = {
        "reviewed_head": head,
        "source_sha256": {str(source.relative_to(root)): hashlib.sha256(original).hexdigest()
                          for source, original, _ in edits},
        "fixture_sha256": {fixture.name: hashlib.sha256(fixture.read_bytes()).hexdigest()
                           for fixture in fixtures},
        "command": command,
        "scope": "real specialist with scripted priced provider; separate real runner with usage mock",
    }
    environment = dict(os.environ, TMPDIR="/var/tmp", CARGO_TARGET_DIR=str(target),
                       CARGO_INCREMENTAL="0", RUSTFLAGS="-D warnings")
    applied = []
    try:
        for source, original, patched in edits:
            if source.read_bytes() != original:
                raise RuntimeError(f"Source changed concurrently: {source}")
            applied.append((source, original, patched))
            source.write_bytes(patched)
        with (out / "probes.log").open("w") as log:
            result = subprocess.run(command, cwd=root, env=environment, stdout=log,
                                    stderr=subprocess.STDOUT, check=False)
        manifest["exit_code"] = result.returncode
    finally:
        conflicts = []
        for source, original, patched in reversed(applied):
            if source.read_bytes() != patched:
                conflicts.append(str(source))
            else:
                source.write_bytes(original)
        if conflicts:
            raise RuntimeError(f"Sources changed concurrently; refusing to overwrite: {conflicts}")
    (out / "probes.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Probe exit status: {result.returncode}; log: {out / 'probes.log'}")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
