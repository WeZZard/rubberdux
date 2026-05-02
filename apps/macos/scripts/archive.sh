#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ARCHIVE_DIR="$PROJECT_DIR/.build/Archives"
CONFIGURATION="${1:-Distribute}"
ARCHIVE_PATH="$ARCHIVE_DIR/Rubberdux-$CONFIGURATION.xcarchive"

echo "Archiving Rubberdux ($CONFIGURATION)..."

xcodebuild archive \
    -project "$PROJECT_DIR/Rubberdux.xcodeproj" \
    -scheme Rubberdux \
    -configuration "$CONFIGURATION" \
    -archivePath "$ARCHIVE_PATH" \
    CODE_SIGN_IDENTITY="-" \
    CODE_SIGN_STYLE="Manual" \
    | tail -3

echo "Archive complete: $ARCHIVE_PATH"
