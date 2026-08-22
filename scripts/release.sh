#!/usr/bin/env bash
# Release packaging script for Concerto.
# Builds release binary, packages .tar.gz, attempts .deb via cargo-deb,
# and emits SHA256 checksums to target/dist/.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$PROJECT_ROOT/target/dist"

echo "==> Building release binary..."
cd "$PROJECT_ROOT"
cargo build --release --workspace

echo "==> Creating dist directory..."
mkdir -p "$DIST_DIR"

# Find the main binary
BINARY="$PROJECT_ROOT/target/release/concerto"
if [ ! -f "$BINARY" ]; then
    echo "ERROR: release binary not found at $BINARY"
    exit 1
fi

# Get version from Cargo.toml
VERSION=$(grep '^version' "$PROJECT_ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
ARCH=$(uname -m)
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
TARBALL_NAME="concerto-${VERSION}-${OS}-${ARCH}.tar.gz"

echo "==> Packaging $TARBALL_NAME..."
cd "$PROJECT_ROOT/target/release"
tar -czf "$DIST_DIR/$TARBALL_NAME" concerto

echo "==> Generating SHA256 checksums..."
cd "$DIST_DIR"
sha256sum "$TARBALL_NAME" > "${TARBALL_NAME}.sha256"

# Attempt .deb via cargo-deb if available
if command -v cargo-deb &>/dev/null; then
    echo "==> Building .deb package..."
    cd "$PROJECT_ROOT"
    cargo deb --no-build --target-dir target || {
        echo "WARNING: .deb build failed, skipping"
    }
else
    echo "==> cargo-deb not found, skipping .deb package"
fi

echo "==> Release artifacts:"
ls -la "$DIST_DIR/"

echo ""
echo "==> Done! Artifacts in $DIST_DIR/"
