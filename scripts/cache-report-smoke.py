#!/usr/bin/env python3
"""Smoke-test CLI replay of a local synthetic session; makes no provider calls."""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time


def smoke(binary):
    binary = Path(binary).resolve(strict=True)
    scratch = Path(__file__).resolve().parents[1] / "target"
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="concerto-cache-smoke-", dir=scratch) as directory:
        root = Path(directory)
        project = root / "project"
        project.mkdir()
        (project / ".concerto.toml").write_text(
            '[cache_pricing."fixture / model"]\n'
            'version = "smoke-v1"\ninput = 3.0\noutput = 15.0\n'
            'cache_read = 0.3\nwrites_charged_as_input = true\n',
            encoding="utf-8",
        )
        environment = dict(os.environ, XDG_DATA_HOME=str(root / "data"),
                           XDG_CONFIG_HOME=str(root / "config"))

        def cli(*arguments):
            return subprocess.run(
                [str(binary), "--project", str(project), "sessions", *arguments],
                env=environment, cwd=project, text=True, capture_output=True,
                check=True, timeout=30,
            ).stdout

        # Let the application initialize and migrate its own isolated database.
        cli("list")
        session_id = "00000000000000000000000001"
        legacy_id = "00000000000000000000000002"
        call = {
            "schema_version": 1, "request_id": session_id,
            "attempt_id": "00000000000000000000000003", "attempt": 1,
            "session_id": session_id, "task_id": session_id,
            "provider": "fixture", "model": "model",
            "system_fingerprint": "system", "history_fingerprint": "history",
            "tools_fingerprint": "tools", "status": "completed",
            "elapsed_ms": 100, "first_content_ms": 20,
            "usage": {"prompt_tokens": 100, "completion_tokens": 10,
                      "cache_read_tokens": 90},
            "estimated_cost_usd": 1.0, "cost_source": "legacy_nominal_estimate",
        }
        with sqlite3.connect(root / "data/concerto/sessions.db") as database:
            for identity in (session_id, legacy_id):
                database.execute(
                    "INSERT INTO sessions (id, created_at, project_dir, provider, model) "
                    "VALUES (?, ?, ?, ?, ?)",
                    (identity, int(time.time()), str(project), "fixture", "model"),
                )

            def event(sequence, kind, record):
                database.execute(
                    "INSERT INTO session_events "
                    "(id, session_id, sequence_num, correlation_id, event_kind, payload, created_at) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?)",
                    (f"{sequence + 10:026d}", session_id, sequence, session_id, kind,
                     json.dumps({kind: {"record": record}}), int(time.time())),
                )

            event(1, "ProviderCallFinished", call)
            database.commit()
            measured = json.loads(cli("cache", session_id, "--json"))
            assert measured["token_hit_rate"] == 0.9, measured
            assert measured["measurement_coverage"] == 1.0, measured
            assert measured["summary"]["missing_cost_calls"] == 0, measured
            assert abs(measured["summary"]["known_estimated_cost_usd"] - 0.000207) < 1e-12
            assert measured["attempts"][0]["cost_source"] == "configured_estimate:smoke-v1"
            event(2, "ProviderCallFinished", call)  # Deduplicate replayed attempts.
            event(3, "ProviderCallStarted", dict(
                call, attempt_id="00000000000000000000000004", status="interrupted",
                usage=None, estimated_cost_usd=None,
            ))

        report = json.loads(cli("cache", session_id, "--json"))
        assert len(report["attempts"]) == 2, report
        assert report["unfinished_attempts"] == 1, report
        assert report["token_hit_rate"] == 0.9, report
        assert report["measurement_coverage"] == 0.5, report
        assert report["summary"]["missing_usage_calls"] == 1, report
        assert report["summary"]["missing_cost_calls"] == 1, report
        caption = cli("cache", session_id)
        assert "90.0% token reuse" in caption and "50.0% coverage" in caption, caption
        assert "Estimated cost: None" in caption, caption
        legacy = json.loads(cli("cache", legacy_id, "--json"))
        assert legacy["attempts"] == [] and legacy["measurement_coverage"] is None, legacy
        assert "N/A" in cli("cache", legacy_id)
    print("CLI cache replay smoke passed (priced, unfinished, duplicate and legacy attempts).")


if __name__ == "__main__":
    smoke(sys.argv[1])
