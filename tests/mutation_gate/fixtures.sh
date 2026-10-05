#!/usr/bin/env bash
# Q03 fixture builders — disposable git workspaces with the Q01 pilot paths
# and package names. Pure helpers: no assertions, no runner invocation.
#
# Every fixture is a two-commit workspace (deterministic baseline + candidate):
# fixed author/committer identity and fixed timestamps make the commits
# reproducible for a given file content.

FIXTURE_EMAIL="fixture@concerto.invalid"
FIXTURE_NAME="Q03 Fixture"
FIXTURE_DATE="2026-10-05T00:00:00+00:00"

# Pilot paths (kept identical to the Q01/Q02 pilot file set).
readonly -a FIXTURE_PILOT_FILES=(
    "crates/core/src/policy.rs"
    "crates/core/src/shell_security.rs"
    "crates/shell/src/parser.rs"
)

# Create the workspace skeleton (baseline tree) at $1. Does not commit.
fixture_new() {
    local repo="$1"
    mkdir -p "$repo/crates/core/src" "$repo/crates/shell/src" || return 1
    git -C "$repo" init -q -b main || return 1
    git -C "$repo" config user.email "$FIXTURE_EMAIL" || return 1
    git -C "$repo" config user.name "$FIXTURE_NAME" || return 1
    git -C "$repo" config commit.gpgsign false || return 1

    cat >"$repo/Cargo.toml" <<'EOF'
[workspace]
resolver = "2"
members = ["crates/core", "crates/shell"]
EOF
    printf '/target\nCargo.lock\n' >"$repo/.gitignore"
    cat >"$repo/crates/core/Cargo.toml" <<'EOF'
[package]
name = "concerto-core"
version = "0.1.0"
edition = "2021"
EOF
    cat >"$repo/crates/shell/Cargo.toml" <<'EOF'
[package]
name = "concerto-shell"
version = "0.1.0"
edition = "2021"
EOF
    cat >"$repo/crates/core/src/lib.rs" <<'EOF'
pub mod policy;
pub mod shell_security;
EOF
    cat >"$repo/crates/shell/src/lib.rs" <<'EOF'
pub mod parser;
EOF
    cat >"$repo/crates/core/src/shell_security.rs" <<'EOF'
pub fn is_allowed(flag: bool) -> bool {
    flag
}

#[cfg(test)]
mod tests {
    use super::is_allowed;

    #[test]
    fn forwards_true() {
        assert!(is_allowed(true));
    }

    #[test]
    fn forwards_false() {
        assert!(!is_allowed(false));
    }
}
EOF
    cat >"$repo/crates/shell/src/parser.rs" <<'EOF'
pub fn count_args(input: &str) -> usize {
    input.split_whitespace().count()
}

#[cfg(test)]
mod tests {
    use super::count_args;

    #[test]
    fn counts_whitespace_separated_args() {
        assert_eq!(count_args("alpha beta gamma"), 3);
        assert_eq!(count_args("   "), 0);
    }
}
EOF
    fixture_write_policy "$repo" weak || return 1
}

# Commit the current tree as a deterministic commit with message $2.
fixture_commit() {
    local repo="$1" msg="$2"
    git -C "$repo" add -A || return 1
    GIT_AUTHOR_DATE="$FIXTURE_DATE" GIT_COMMITTER_DATE="$FIXTURE_DATE" \
        git -C "$repo" commit -q -m "$msg" || return 1
}

fixture_head() {
    git -C "$1" rev-parse HEAD
}

# An intentionally empty candidate commit (no tree change) for the empty-diff
# contract case.
fixture_commit_empty() {
    local repo="$1" msg="$2"
    GIT_AUTHOR_DATE="$FIXTURE_DATE" GIT_COMMITTER_DATE="$FIXTURE_DATE" \
        git -C "$repo" commit -q --allow-empty -m "$msg" || return 1
}

# The strong variant is verified against real cargo-mutants 27.1.0: every
# mutant of classify() is killed by the boundary assertions (caught case).
fixture_write_policy() {
    local repo="$1" variant="$2" path="$1/crates/core/src/policy.rs"
    case "$variant" in
        weak)
            cat >"$path" <<'EOF'
pub fn classify(v: i64) -> &'static str {
    if v < 0 {
        "neg"
    } else if v == 0 {
        "zero"
    } else {
        "pos"
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn smoke_pos() {
        assert_eq!(classify(7), "pos");
    }
}
EOF
            ;;
        strong)
            cat >"$path" <<'EOF'
pub fn classify(v: i64) -> &'static str {
    if v < 0 {
        "neg"
    } else if v == 0 {
        "zero"
    } else {
        "pos"
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn classifies_values() {
        assert_eq!(classify(-1), "neg");
        assert_eq!(classify(0), "zero");
        assert_eq!(classify(7), "pos");
    }
}
EOF
            ;;
        broken)
            # Baseline-run failure material: tests compile but fail.
            cat >"$path" <<'EOF'
pub fn classify(v: i64) -> &'static str {
    if v < 0 {
        "neg"
    } else if v == 0 {
        "zero"
    } else {
        "pos"
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn classifies_values() {
        assert_eq!(classify(-1), "neg");
        assert_eq!(classify(0), "impossible baseline assertion");
        assert_eq!(classify(7), "pos");
    }
}
EOF
            ;;
        slow)
            # Strong assertions plus a >=2.5s test so a 1s budget kill and a
            # mid-run signal always land while the tool is still running.
            cat >"$path" <<'EOF'
pub fn classify(v: i64) -> &'static str {
    if v < 0 {
        "neg"
    } else if v == 0 {
        "zero"
    } else {
        "pos"
    }
}

#[cfg(test)]
mod tests {
    use super::classify;

    #[test]
    fn classifies_values() {
        assert_eq!(classify(-1), "neg");
        assert_eq!(classify(0), "zero");
        assert_eq!(classify(7), "pos");
    }

    #[test]
    fn baseline_takes_long_enough_to_observe() {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        assert_eq!(classify(7), "pos");
    }
}
EOF
            ;;
        *)
            printf 'fixture_write_policy: unknown variant %s\n' "$variant" >&2
            return 1
            ;;
    esac
}

# Non-pilot candidate edit used by the out-of-pilot case.
fixture_write_lib_candidate() {
    local repo="$1"
    cat >"$repo/crates/core/src/lib.rs" <<'EOF'
pub mod policy;
pub mod shell_security;

pub fn crate_version() -> u8 {
    1
}
EOF
}

# Digest of the pilot sources (+ HEAD + tracked status) so cases can prove the
# runner never edits the candidate checkout. Missing files record as ABSENT.
snapshot_source_state() {
    local repo="$1" f
    printf 'HEAD=%s\n' "$(git -C "$repo" rev-parse HEAD)"
    printf 'tracked-status:\n%s\n' "$(git -C "$repo" status --porcelain --untracked-files=no)"
    for f in "${FIXTURE_PILOT_FILES[@]}"; do
        if [ -f "$repo/$f" ]; then
            sha256sum "$repo/$f"
        else
            printf 'ABSENT  %s\n' "$f"
        fi
    done
}
