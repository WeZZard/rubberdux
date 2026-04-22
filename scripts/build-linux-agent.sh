#!/bin/bash
set -euo pipefail

# Build the rubberdux agent binary for Linux (aarch64 musl).
# This binary is baked into VM images during `cargo build`.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
TARGET="aarch64-unknown-linux-musl"

echo "Building rubberdux agent for Linux ($TARGET)..."

# 1. Ensure cross-compilation linker is available
if ! command -v aarch64-linux-musl-gcc &> /dev/null; then
    echo "Error: aarch64-linux-musl-gcc not found."
    echo ""
    echo "Install it with Homebrew:"
    echo "  brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl"
    exit 1
fi

# 2. Ensure .cargo/config.toml points to the linker
CARGO_CONFIG="$PROJECT_DIR/.cargo/config.toml"
if [[ ! -f "$CARGO_CONFIG" ]]; then
    echo "Creating $CARGO_CONFIG..."
    mkdir -p "$(dirname "$CARGO_CONFIG")"
    cat > "$CARGO_CONFIG" <<EOF
[target.aarch64-unknown-linux-musl]
linker = "aarch64-linux-musl-gcc"
EOF
fi

# 3. Ensure Rust target is installed
if ! rustup target list --installed | grep -q "^${TARGET}$"; then
    echo "Installing Rust target $TARGET..."
    rustup target add "$TARGET"
fi

# 4. Build
cd "$PROJECT_DIR"

# Ensure we use rustup's cargo/rustc, not Homebrew's
# Homebrew's rustc may have a different version than rustup's
RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}"
export RUSTUP_TOOLCHAIN

# Ensure rustup's bin directory is first in PATH
RUSTUP_BIN="$(rustup which rustc | xargs dirname)"
export PATH="$RUSTUP_BIN:$PATH"

# Use a separate target directory to avoid deadlocking with the parent cargo
# process that invoked this script (e.g. from build.rs).
export CARGO_TARGET_DIR="$PROJECT_DIR/target/linux-agent-build"

echo "Using rustc: $(rustc --version)"
echo "Using toolchain: $RUSTUP_TOOLCHAIN"
echo "Using CARGO_TARGET_DIR: $CARGO_TARGET_DIR"

cargo build --no-default-features --features agent --target "$TARGET" --release

echo ""
echo "Linux agent binary ready:"
echo "  target/$TARGET/release/rubberdux"
