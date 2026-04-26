#!/bin/bash
# Launch Zed with the correct Rust toolchain for building extensions
# This ensures rustup's cargo (with WASM support) is used instead of Homebrew's

set -e

# Ensure we use rustup's cargo
export PATH="$HOME/.cargo/bin:$PATH"

# Verify cargo is from rustup
CARGO_PATH=$(which cargo)
if [[ "$CARGO_PATH" == "/opt/homebrew/bin/cargo" ]]; then
    echo "Error: Still finding Homebrew's cargo. Please ensure rustup is installed."
    echo "Visit: https://rustup.rs"
    exit 1
fi

echo "Using cargo: $CARGO_PATH"
echo "Launching Zed..."

# Launch Zed from current directory
zed .
