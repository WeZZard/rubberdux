#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/../../../.." && pwd)"
cd "$PROJECT_DIR"

SESSION_DIR="${RUBBERDUX_SESSION_DIR:-./sessions}"
PID_FILE="$SESSION_DIR/rubberdux.pid"

# Stop the running process
if [ -f "$PID_FILE" ]; then
    PID=$(cat "$PID_FILE")
    if kill -0 "$PID" 2>/dev/null; then
        echo "Stopping rubberdux (PID $PID)..."
        kill "$PID" 2>/dev/null || true
        sleep 1
        if kill -0 "$PID" 2>/dev/null; then
            echo "Force killing..."
            kill -9 "$PID" 2>/dev/null || true
        fi
        echo "Stopped."
    else
        echo "Process $PID is not running."
    fi
    rm -f "$PID_FILE"
else
    echo "No PID file found. Nothing to stop."
fi

# Clean up session artifacts
CLEANED=0

if [ -f "$SESSION_DIR/launch.log" ]; then
    rm -f "$SESSION_DIR/launch.log"
    CLEANED=$((CLEANED + 1))
fi

if [ -d "$SESSION_DIR/tasks" ]; then
    COUNT=$(find "$SESSION_DIR/tasks" -type f | wc -l | tr -d ' ')
    rm -rf "$SESSION_DIR/tasks"
    echo "Removed tasks/ ($COUNT files)"
    CLEANED=$((CLEANED + COUNT))
fi

if [ -d "$SESSION_DIR/subagents" ]; then
    COUNT=$(find "$SESSION_DIR/subagents" -type f | wc -l | tr -d ' ')
    rm -rf "$SESSION_DIR/subagents"
    echo "Removed subagents/ ($COUNT files)"
    CLEANED=$((CLEANED + COUNT))
fi

if [ -d "$SESSION_DIR/tool-results" ]; then
    COUNT=$(find "$SESSION_DIR/tool-results" -type f | wc -l | tr -d ' ')
    rm -rf "$SESSION_DIR/tool-results"
    echo "Removed tool-results/ ($COUNT files)"
    CLEANED=$((CLEANED + COUNT))
fi

if [ "$CLEANED" -gt 0 ]; then
    echo "Cleaned $CLEANED artifact(s)."
else
    echo "No artifacts to clean."
fi
