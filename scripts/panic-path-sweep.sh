#!/usr/bin/env bash
# Panic-Path Sweep (Gate 6)
# Scans for unchecked arithmetic and indexing that could panic on empty/zero lengths.
# Run before merging any "hardening" or "quality" branch.
# Usage: bash scripts/panic-path-sweep.sh [--checklist]
set -euo pipefail

echo "=== Gate 6: Panic-Path Sweep ==="
echo ""

patterns=(
    "checked_sub(1)"
    "checked_add(1)"
    "unwrap_or(.*\.len()"
    "\.len()\s*-\s*1"
    "% \w+\.len()"
    "% \w+\.len\b"
    "\[[a-z_]+\.len()\s*-\s*1\]"
)

echo "--- Sites found ---"
for pat in "${patterns[@]}"; do
    echo ""
    echo "Pattern: $pat"
    grep -rn "$pat" --include='*.rs' crates/ 2>/dev/null || echo "(none)"
done

echo ""
echo "=== Verification Checklist ==="
echo "Each site below must be marked SAFE (guard present or provably non-empty)"
echo "or COVERED (test exists for empty/zero case). Mark any UNSAFE sites."
echo ""

# Known safe sites from last sweep (update after each run)
cat << 'EOF'
| File | Line | Pattern | Status | Justification |
|------|------|---------|--------|---------------|
| editor_core.rs | 341 | find_matches.len() - 1 | SAFE | guarded by !find_matches.is_empty() |
| editor_core.rs | 492 | checked_sub(1).unwrap_or(completion_items.len() - 1) | SAFE | guarded by !completion_items.is_empty() |
| studio/mod.rs | 530 | find_matches.len() - 1 | SAFE | guarded by early return on empty |
| agent_graph.rs | 135 | nodes.len() - 1 | SAFE | test-only, built from roles param |
| diff.rs | 58 | changes.len() - 1 | SAFE | changes.last().is_some_and() short-circuits on empty |
| context_compaction.rs | 179 | children[children.len() - 1] | SAFE | merge_group has >= merge_width (>=2) elements |
| planner.rs | 192 | coder_indices.len() | SAFE | early return ensures >=1 Coder |
| cli/app.rs | 821 | model_choices.len() - 1 | SAFE | guarded by !model_choices.is_empty(); regression tested |
| cli/app.rs | 880 | model_choices.len() - 1 | SAFE | guarded by model_choices.is_empty() check at 871 |
| cli/app.rs | 921 | settings.len() - 1 | SAFE | guarded by settings.is_empty() at 900 |
| cli/app.rs | 923 | settings.len() | SAFE | guarded by settings.is_empty() at 900 |
| cli/app.rs | 957 | AgentMode::ALL.len() - 1 | SAFE | static non-empty array |
| cli/app.rs | 959 | AgentMode::ALL.len() | SAFE | static non-empty array |
| context_compaction.rs | 380 | children.len().max(1) | SAFE | .max(1) prevents division by zero |
| context_guard.rs | 364 | messages.len() | SAFE | used as insert position, safe on empty vec |
EOF

echo ""
echo "=== Done ==="