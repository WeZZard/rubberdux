#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/../../../.." && pwd)"
cd "$PROJECT_DIR"

SESSION_DIR="${RUBBERDUX_SESSION_DIR:-./sessions}"
LOG_FILE="$SESSION_DIR/launch.log"
PID_FILE="$SESSION_DIR/rubberdux.pid"

mkdir -p "$SESSION_DIR"

# Stop any existing instance
if [ -f "$PID_FILE" ]; then
    OLD_PID=$(cat "$PID_FILE")
    if kill -0 "$OLD_PID" 2>/dev/null; then
        echo "Stopping existing instance (PID $OLD_PID)..."
        kill "$OLD_PID" 2>/dev/null || true
        sleep 1
        kill -9 "$OLD_PID" 2>/dev/null || true
    fi
    rm -f "$PID_FILE"
fi

# Archive previous session
if [ -f "$SESSION_DIR/session.jsonl" ]; then
    ARCHIVE_NAME="session.$(date +%Y%m%d_%H%M%S).jsonl"
    mv "$SESSION_DIR/session.jsonl" "$SESSION_DIR/$ARCHIVE_NAME"
    echo "Archived previous session to $ARCHIVE_NAME"
fi

# Build
echo "Building rubberdux..."
cargo build --release 2>&1
echo "Build succeeded."

# Launch as background process
echo "Launching rubberdux (log: $LOG_FILE)..."
nohup cargo run --release -- --host > "$LOG_FILE" 2>&1 &
CHILD_PID=$!
echo "$CHILD_PID" > "$PID_FILE"
echo "rubberdux started (PID $CHILD_PID)"

# Wait briefly for startup, then tail initial log
sleep 2
if kill -0 "$CHILD_PID" 2>/dev/null; then
    echo "--- Startup log (first 50 lines) ---"
    head -n 50 "$LOG_FILE" 2>/dev/null || true
    echo "--- End startup log ---"
    echo ""
    echo "rubberdux is running. PID: $CHILD_PID"
    echo "Full log: $LOG_FILE"
    echo "Stop with: kill $CHILD_PID"
else
    echo "rubberdux exited immediately. Full log:"
    cat "$LOG_FILE" 2>/dev/null || true
    exit 1
fi
