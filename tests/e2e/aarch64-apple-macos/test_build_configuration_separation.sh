#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../../../apps/macos" && pwd)"
DERIVED_DATA="$PROJECT_DIR/.build/DerivedData"

PASS=0
FAIL=0

assert_plist_value() {
    local app_path="$1"
    local key="$2"
    local expected="$3"
    local plist="$app_path/Contents/Info.plist"

    if [ ! -f "$plist" ]; then
        echo "  FAIL: $plist does not exist"
        FAIL=$((FAIL + 1))
        return
    fi

    local actual
    actual=$(/usr/libexec/PlistBuddy -c "Print :$key" "$plist" 2>/dev/null || echo "(not found)")

    if [ "$actual" = "$expected" ]; then
        echo "  PASS: $key = $actual"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $key = $actual (expected: $expected)"
        FAIL=$((FAIL + 1))
    fi
}

for config in Debug Release Distribute; do
    echo "Building $config..."
    xcodebuild build \
        -project "$PROJECT_DIR/Rubberdux.xcodeproj" \
        -scheme Rubberdux \
        -configuration "$config" \
        -derivedDataPath "$DERIVED_DATA" \
        -quiet
done

echo ""
echo "=== Debug ==="
assert_plist_value "$DERIVED_DATA/Build/Products/Debug/Rubberdux (Debug).app" \
    CFBundleIdentifier "com.wezzard.rubberdux.debug"
assert_plist_value "$DERIVED_DATA/Build/Products/Debug/Rubberdux (Debug).app" \
    CFBundleDisplayName "Rubberdux (Debug)"

echo "=== Release ==="
assert_plist_value "$DERIVED_DATA/Build/Products/Release/Rubberdux (Release).app" \
    CFBundleIdentifier "com.wezzard.rubberdux.release"
assert_plist_value "$DERIVED_DATA/Build/Products/Release/Rubberdux (Release).app" \
    CFBundleDisplayName "Rubberdux (Release)"

echo "=== Distribute ==="
assert_plist_value "$DERIVED_DATA/Build/Products/Distribute/Rubberdux.app" \
    CFBundleIdentifier "com.wezzard.rubberdux"
assert_plist_value "$DERIVED_DATA/Build/Products/Distribute/Rubberdux.app" \
    CFBundleDisplayName "Rubberdux"

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
