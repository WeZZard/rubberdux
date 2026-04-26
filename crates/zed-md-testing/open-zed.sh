#!/bin/bash
# Launch Zed with rustup's PATH to ensure dev extensions compile correctly

set -e

# Ensure rustup's cargo and rustc are first in PATH
export PATH="$HOME/.cargo/bin:$PATH"

# Verify which cargo/rustc we'll use
echo "Cargo: $(which cargo)"
echo "Rustc: $(which rustc)"
echo "Rustc version: $(rustc --version)"

# Check for wasm32-wasip2 target
if ! rustup target list --installed | grep -q "wasm32-wasip2"; then
    echo "Installing wasm32-wasip2 target..."
    rustup target add wasm32-wasip2
fi

echo "WASM target: installed"
echo ""
echo "Launching Zed..."
echo ""

# Launch Zed with current directory
open -a Zed "$@"
