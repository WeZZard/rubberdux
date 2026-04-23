#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/../../../.." && pwd)"
cd "$PROJECT_DIR"

PID_FILE="$PROJECT_DIR/rubberdux.pid"

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

# Stop any Tart VMs
VM_COUNT=0
if command -v tart &> /dev/null; then
    while IFS= read -r line; do
        if echo "$line" | grep -q "rubberdux-"; then
            VM_NAME=$(echo "$line" | awk '{print $2}')
            echo "Stopping VM: $VM_NAME"
            tart stop "$VM_NAME" 2>/dev/null || true
            VM_COUNT=$((VM_COUNT + 1))
        fi
    done < <(tart list 2>/dev/null | grep "running" || true)
fi

if [ "$VM_COUNT" -gt 0 ]; then
    echo "Stopped $VM_COUNT VM(s)."
fi

echo "Cleanup complete."
