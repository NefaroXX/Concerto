"""Self-check fixture construction and reject false-positive report evidence.

These checks need Python and Git, not Rust; they are not runner acceptance.
"""
import copy
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import unittest

import fake_cargo
import harness as h


class HarnessTests(unittest.TestCase):
    def setUp(self):
        # Explicit disk-backed parent; never tempfile's default /tmp.
        parent = Path(os.environ.get("Q03_SELF_TEST_ROOT", Path.cwd())).resolve()
        if parent.is_relative_to(Path("/tmp").resolve()):
            raise RuntimeError("self-checks must run outside /tmp")
        self.temp = tempfile.TemporaryDirectory(prefix="q03-self-", dir=parent)
        self.addCleanup(self.temp.cleanup)
        self.area = Path(self.temp.name)

    def report(self):
        state = h.fixture(self.area / "repo")
        report_dir = self.area / "report"
        output = report_dir / "core/mutants.out"
        fake_cargo.emit(output, [fake_cargo.mutant(h.PILOT[0], 0)], "caught")
        (report_dir / "console.log").write_text("simulated verifier input\n")
        report = {"schema_version": 1, "contract_id": h.CONTRACT, "tool_version": h.PIN,
                  "base_sha": state["base"], "head_sha": state["head"],
                  "merge_base_sha": state["merge_base"], "selected_files": state["selected"],
                  "uncovered_files": [], "skipped_files": [], "coverage_scope": "pilot_only",
                  "status": "passed", "reason": "completed fixture", "exit_code": 0,
                  "tool_exit_codes": [0], "counts": {"generated": 1, "caught": 1, "missed": 0,
                                                       "timeout": 0, "unviable": 0},
                  "complete": True, "duration_seconds": 1.0, "budget_seconds": 10,
                  "artifact_paths": ["console.log", "core/mutants.out/outcomes.json"]}
        return state, report_dir, report

    def check(self, state, directory, report):
        (directory / "gate-report.json").write_text(json.dumps(report))
        return h.verify_report(directory, 0, 0, "passed", state)

    def test_fixture_commits_include_only_selected_pilot_change(self):
        """The source under test is a clean committed candidate, not a dirty overlay."""
        root = self.area / "repo with spaces"
        state = h.fixture(root)
        changed = h.git(root, "diff", "--name-only", "-z", state["base"], state["head"])
        self.assertEqual(changed, (h.PILOT[0] + "\0").encode())
        self.assertEqual(h.git(root, "status", "--porcelain=v1", "-z"), b"")

    def test_merge_base_fixture_really_diverges(self):
        """An explicit base tip must not become a two-dot diff that includes base-only work."""
        root = self.area / "repo"
        state = h.fixture(root, "diverged")
        self.assertNotEqual(state["base"], state["merge_base"])
        self.assertEqual(h.git(root, "merge-base", state["base"], state["head"]).decode().strip(), state["merge_base"])
        self.assertFalse((root / "crates/core/src/base_only.rs").exists())

    def test_odd_filename_survives_git_without_word_splitting(self):
        """Whitespace, Unicode, metacharacters and a newline remain a single Git path."""
        root = self.area / "repo"
        state = h.fixture(root, "outside")
        self.assertEqual(h.git(root, "diff", "--name-only", "-z", state["base"], state["head"]), (h.ODD + "\0").encode())

    def test_snapshot_notices_ignored_source_and_symlink_changes(self):
        """Git status alone cannot prove ignored inputs and symlink targets were preserved."""
        root = self.area / "repo"
        h.fixture(root)
        before = h.snapshot(root)
        h.write(root, ".hidden-input", "changed")
        self.assertNotEqual(h.snapshot(root), before)
        (root / "alias").symlink_to(".hidden-input")
        first = h.snapshot(root)
        (root / "alias").unlink()
        (root / "alias").symlink_to("Cargo.toml")
        self.assertNotEqual(h.snapshot(root), first)

    def test_report_accepts_attributed_completed_evidence(self):
        """A valid envelope with matching raw baseline and per-file outcomes is readable."""
        state, directory, report = self.report()
        self.check(state, directory, report)

    def test_report_rejects_false_pass_variants(self):
        """Incomplete, timed-out, empty, unpinned and malformed reports cannot turn green."""
        state, directory, good = self.report()
        corruptions = [
            lambda r: r.update(complete=False),
            lambda r: r.update(selected_files=[]),
            lambda r: r.update(tool_version="0.0.0"),
            lambda r: r.update(head_sha="0" * 40),
            lambda r: r.update(duration_seconds=float("nan")),
            lambda r: r["counts"].update(caught=True),
            lambda r: r["counts"].update(caught=0, unviable=1),
            lambda r: r["counts"].update(timeout=1),
            lambda r: r.update(artifact_paths=["../outside.log"]),
            lambda r: r.pop("coverage_scope"),
        ]
        for corrupt in corruptions:
            with self.subTest(change=corruptions.index(corrupt)):
                report = copy.deepcopy(good)
                corrupt(report)
                with self.assertRaises(h.CheckFailed):
                    self.check(state, directory, report)

    def test_global_count_cannot_hide_missing_per_file_evidence(self):
        """At least one viable caught mutant is required in every selected file."""
        state, directory, report = self.report()
        state["selected"] = list(h.PILOT[:2])
        report["selected_files"] = state["selected"]
        with self.assertRaisesRegex(h.CheckFailed, "each selected file"):
            self.check(state, directory, report)

    def test_raw_missed_result_overrules_claimed_pass(self):
        """A runner cannot hide a surviving mutant behind fabricated summary counts."""
        state, directory, report = self.report()
        fake_cargo.emit(directory / "core/mutants.out", [fake_cargo.mutant(h.PILOT[0], 0)], "missed")
        with self.assertRaisesRegex(h.CheckFailed, "contradict pass"):
            self.check(state, directory, report)

    def test_work_root_preserves_existing_evidence_and_rejects_source(self):
        """A rerun cannot delete previous evidence or create scratch inside the source."""
        (self.area / "evidence.txt").write_text("keep")
        with self.assertRaises(h.CheckFailed):
            h.prepare_work(self.area)
        self.assertEqual((self.area / "evidence.txt").read_text(), "keep")
        with self.assertRaises(h.CheckFailed):
            h.prepare_work(h.HERE / "must-not-be-created")

    def test_process_watchdog_reaps_its_own_group(self):
        """The harness failure path cannot leave its test child running."""
        process = subprocess.Popen(["sleep", "60"], start_new_session=True)
        self.addCleanup(lambda: h.stop_group(process))
        self.assertTrue(h.alive(process.pid))
        h.stop_group(process)
        self.assertIsNotNone(process.poll())
        self.assertFalse(h.alive(process.pid))


if __name__ == "__main__":
    unittest.main()
