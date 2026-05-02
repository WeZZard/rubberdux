#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DERIVED_DATA="$PROJECT_DIR/.build/DerivedData"
CONFIGURATION="${1:-Debug}"

echo "Building Rubberdux ($CONFIGURATION)..."

xcodebuild build \
    -project "$PROJECT_DIR/Rubberdux.xcodeproj" \
    -scheme Rubberdux \
    -configuration "$CONFIGURATION" \
    -derivedDataPath "$DERIVED_DATA" \
    | tail -3

echo "Build complete: $DERIVED_DATA/Build/Products/$CONFIGURATION/"
