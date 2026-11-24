#!/usr/bin/env python3
"""Require a complete Cargo audit, including successful registry checks.

cargo-audit 0.22.2 can print registry errors and still exit zero. Keep its
advisory policy and reject error/warning diagnostics independently of its exit
code so a partial scan cannot turn the CI gate green.
"""

import re
import subprocess
import sys


AUDIT_COMMAND = ["cargo", "audit", "--deny", "warnings", "--color", "never"]
ANSI_COLOR = re.compile(r"\x1b\[[0-9;]*m")
DIAGNOSTIC = re.compile(r"^\s*(?:error|warning)(?:\[[^]]*\])?:", re.IGNORECASE)


def run_audit(command=None):
    """Run the audit and preserve its diagnostics; return a CI exit code."""
    try:
        result = subprocess.run(
            AUDIT_COMMAND if command is None else command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except OSError as error:
        print(f"error: unable to run cargo-audit: {error}", file=sys.stderr)
        return 2

    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    if result.returncode:
        return result.returncode if result.returncode > 0 else 1
    output = ANSI_COLOR.sub("", result.stdout + "\n" + result.stderr)
    if any(DIAGNOSTIC.match(line) for line in output.splitlines()):
        print("error: cargo-audit reported diagnostics despite exit zero; audit is incomplete or unclean", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(run_audit())
