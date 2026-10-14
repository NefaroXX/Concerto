import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("cache_benchmark", Path(__file__).parents[1] / "cache-benchmark.py")
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


def run(total, reads, cost=2):
    return {"task_id": "same-task", "task_description": "fixture", "completed": True,
            "tests_passing_after": True, "files_correctly_modified": 1,
            "time_to_complete_ms": 100, "interventions": 0,
            "telemetry": {"cache": {"calls": 1, "cache_reporting_calls": 1,
                                    "calls_with_hits": int(reads > 0), "invalid_cache_calls": 0,
                                    "cache_read_tokens": reads, "measured_input_tokens": total,
                                    "cache_write_tokens": None},
                          "missing_usage_calls": 0, "missing_cost_calls": int(cost is None),
                          "failed_attempts": 0, "interrupted_attempts": 0,
                          "known_estimated_cost_usd": cost or 0,
                          "reported_input_tokens": total, "reported_output_tokens": 10}}


def report(*runs):
    return {"individual_results": list(runs), "metadata": {
        "runtime_mode": "single_agent_fixture", "policy_mode": "permissive_fixture",
        "pricing_fingerprint": "fixture-price-v1", "cost_source": "configured_estimate"}}


class ReportingTests(unittest.TestCase):
    def test_ratio_of_sums_and_all_attempt_cost_per_success(self):
        failed = run(900, 0, cost=3)
        failed["completed"] = False
        summary = cache.summarize(report(run(100, 90), failed))
        self.assertEqual(summary["token_hit_rate"], .09)
        self.assertEqual(summary["verified_success_rate"], .5)
        self.assertEqual(summary["cost_per_verified_success_usd"], 5)
        self.assertIsNone(cache.summarize(report())["token_hit_rate"])

    def test_missing_cost_does_not_hide_complete_tokens(self):
        result = cache.compare(report(run(100, 90)), report(run(100, 90, None)))
        self.assertFalse(result["costs_comparable"])
        self.assertTrue(result["tokens_comparable"])
        self.assertIsNone(result["candidate"]["cost_per_verified_success_usd"])

    def test_legacy_and_different_prices_are_not_savings(self):
        legacy = run(100, 90)
        legacy.pop("telemetry")
        self.assertFalse(cache.compare(report(legacy), report(run(100, 90)))["tokens_comparable"])
        candidate = report(run(100, 90))
        candidate["metadata"]["pricing_fingerprint"] = "v2"
        self.assertFalse(cache.compare(report(run(100, 90)), candidate)["costs_comparable"])

    def test_different_tasks_are_not_comparable(self):
        candidate = run(100, 90)
        candidate["task_id"] = "another-task"
        self.assertFalse(cache.compare(report(run(100, 90)), report(candidate))["tokens_comparable"])


class ExportTests(unittest.TestCase):
    def test_examples_require_verification_deduplicate_and_keep_project_lineage(self):
        row = {"project_id": "project", "task_id": "task", "session_id": "session",
               "visible_input": "password=supersecret", "visible_output": "fixed",
               "verification": "external:test:pass", "externally_verified": True,
               "approved_for_export": True, "private_reasoning": "must be discarded"}
        other = dict(row, task_id="other", session_id="other")
        export = cache.curate_examples([row, row, other, dict(row, externally_verified=False)])
        self.assertEqual(len(export["examples"]), 2)
        self.assertEqual(len(export["rejected"]), 1)
        self.assertEqual(export["examples"][0]["split"], export["examples"][1]["split"])
        self.assertTrue(export["examples"][0]["redaction_applied"])
        self.assertNotIn("private_reasoning", export["examples"][0])
        self.assertNotIn("supersecret", json.dumps(export))

    def test_cases_copy_snapshot_and_reject_credentials_before_writing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            task_dir = root / "suite" / "task"
            task_dir.mkdir(parents=True)
            task = {"id": "task", "description": "fix addition", "project_snapshot_path": ".",
                    "expected_outcomes": [{"files_changed": ["lib.rs"], "tests_pass": True,
                                           "min_patterns": ["a+b"], "max_patterns": []}]}
            (task_dir / "task.json").write_text(json.dumps(task))
            (task_dir / "lib.rs").write_text("a+b")
            cache.export_cases(task_dir.parent, root / "export", "project")
            self.assertEqual((root / "export/task/snapshot/lib.rs").read_text(), "a+b")
            (task_dir / ".env").write_text("password=supersecret")
            with self.assertRaises(ValueError):
                cache.export_cases(task_dir.parent, root / "rejected", "project")
            self.assertFalse((root / "rejected").exists())


if __name__ == "__main__":
    unittest.main()
