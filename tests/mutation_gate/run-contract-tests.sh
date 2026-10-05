#!/usr/bin/env bash
# Q03 black-box checks of the runner supplied by Q02. Does not edit that runner.
set -euo pipefail
script_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$script_dir/harness.py" "$@"
