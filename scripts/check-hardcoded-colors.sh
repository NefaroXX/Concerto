#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Check for hard-coded colors in the desktop UI source.
#
# Rule: Color::from_rgb / from_rgba and Color::BLACK/WHITE/TRANSPARENT
# must NOT appear in views/ or ui/ (these must use palette colors).
# Widget code (widgets/) is excluded — those are lower-level canvas/widgets
# that receive palette colors as parameters.
#
# Usage:
#   ./scripts/check-hardcoded-colors.sh
#
# Exit code 0 = clean, 1 = violations found.
# ---------------------------------------------------------------------------

set -euo pipefail

DESKTOP_SRC="crates/desktop/src"
EXCLUDE_PATTERN="^crates/desktop/src/theme|^crates/desktop/src/widgets"
violations=0

check_pattern() {
    local label="$1"
    local pattern="$2"

    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        file=$(echo "$line" | cut -d: -f1)
        echo "$file" | grep -qE "$EXCLUDE_PATTERN" && continue
        # Skip doc-comment and comment lines
        content=$(echo "$line" | sed 's/^[^:]*:[^:]*://')
        echo "$content" | grep -qE '^\s*///?\s*' && continue
        echo "VIOLATION [${label}]: $line"
        violations=$((violations + 1))
    done < <(grep -rn "$pattern" "$DESKTOP_SRC" 2>/dev/null || true)
}

check_pattern "from_rgb"  'Color::from_rgb('
check_pattern "from_rgba" 'Color::from_rgba('
check_pattern "constants" 'Color::BLACK\|Color::WHITE\|Color::TRANSPARENT'
# Also catch hex color strings in Rust source (e.g. "#fff" or "#ffffff")
check_pattern "hex-color" '"[#][0-9a-fA-F]\{3\}\b\|"[#][0-9a-fA-F]\{6\}\b'

if [[ $violations -gt 0 ]]; then
    echo ""
    echo "FAILED: $violations hard-coded color(s) found outside theme/ module."
    echo "Use palette colors (theme.palette.*) or ThemeExt helpers instead."
    exit 1
fi

echo "OK: No hard-coded colors found outside theme/ module."
exit 0
