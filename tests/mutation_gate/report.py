#!/usr/bin/env python3
"""Read-only gate-report.json helper for the Q03 contract harness.

Subcommands:
  get <report.json> <dotted.key>        print the value (scalars raw, null as
                                        "null", arrays/objects as compact JSON)
  contains <report.json> <key> <needle> array membership or string substring
  summary <report.json>                 one-line status/reason/counts digest

Exit codes:
  0  ok / contains
  1  not contained
  3  report missing or unreadable
  4  key missing from the report
  2  usage error
"""

from __future__ import annotations

import json
import sys


def load(path: str):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None


def walk(doc, dotted: str):
    cur = doc
    for part in dotted.split("."):
        if not isinstance(cur, dict) or part not in cur:
            raise KeyError(dotted)
        cur = cur[part]
    return cur


def fmt(value) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (str, int, float)):
        return str(value)
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def cmd_get(argv) -> int:
    if len(argv) != 2:
        return 2
    doc = load(argv[0])
    if doc is None:
        return 3
    try:
        value = walk(doc, argv[1])
    except KeyError:
        return 4
    sys.stdout.write(fmt(value) + "\n")
    return 0


def cmd_contains(argv) -> int:
    if len(argv) != 3:
        return 2
    doc = load(argv[0])
    if doc is None:
        return 3
    try:
        value = walk(doc, argv[1])
    except KeyError:
        return 4
    needle = argv[2]
    if isinstance(value, list):
        return 0 if needle in [str(item) for item in value] else 1
    if isinstance(value, str):
        return 0 if needle in value else 1
    return 1


def cmd_summary(argv) -> int:
    if len(argv) != 1:
        return 2
    doc = load(argv[0])
    if doc is None:
        return 3
    counts = doc.get("counts")
    if not isinstance(counts, dict):
        counts = {}
    parts = [
        "status={}".format(doc.get("status")),
        "reason={}".format(doc.get("reason")),
        "exit_code={}".format(fmt(doc.get("exit_code"))),
        "complete={}".format(fmt(doc.get("complete"))),
        "counts=(generated={},caught={},missed={},timeout={},unviable={})".format(
            fmt(counts.get("generated")),
            fmt(counts.get("caught")),
            fmt(counts.get("missed")),
            fmt(counts.get("timeout")),
            fmt(counts.get("unviable")),
        ),
    ]
    sys.stdout.write("report: " + " ".join(parts) + "\n")
    return 0


def main(argv) -> int:
    if len(argv) < 2:
        sys.stderr.write("usage: report.py get|contains|summary <args>\n")
        return 2
    cmd, rest = argv[0], argv[1:]
    if cmd == "get":
        return cmd_get(rest)
    if cmd == "contains":
        return cmd_contains(rest)
    if cmd == "summary":
        return cmd_summary(rest)
    sys.stderr.write("report.py: unknown subcommand {}\n".format(cmd))
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
