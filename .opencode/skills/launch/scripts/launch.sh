#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/../../../.." && pwd)"
cd "$PROJECT_DIR"

# Use cargo xtask launch for full provisioning + build + run
echo "Launching rubberdux via xtask..."
cargo xtask launch
