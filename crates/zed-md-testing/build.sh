#!/bin/bash
# Build script for zed-md-testing extension
# This ensures we use rustup's cargo, not Homebrew's

set -e

# Use rustup's cargo explicitly
export PATH="$HOME/.cargo/bin:$PATH"

# Verify we're using rustup cargo
CARGO="$HOME/.cargo/bin/cargo"
echo "Using cargo: $($CARGO --version)"
echo "Using rustc: $($HOME/.cargo/bin/rustc --version)"

# Build the extension for WASM target
echo "Building extension for wasm32-wasip1..."
$CARGO build --release --target wasm32-wasip1

# Copy the built WASM file to the extension root
EXTENSION_DIR="$(cd "$(dirname "$0")" && pwd)"
cp "$EXTENSION_DIR/target/wasm32-wasip1/release/zed_md_testing.wasm" \
   "$EXTENSION_DIR/extension.wasm"

echo "Build complete! Extension is at: $EXTENSION_DIR/extension.wasm"
