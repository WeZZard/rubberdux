#!/bin/bash
# VC-E.2 — the three fluid modes project from input ORIGIN per edge (Inv 19).
#
# End-to-end runner: boots the SAME proven live stack as V-system-drive (in-process
# host + a real `rubberduxd --agent` worker + SurfaceRouter + the macOS surface
# client) via the `system` test target's `app::surface_modes` case, drives a benign
# turn and one eliciting `set_value` turn against the REAL model, and asserts from
# the worker's `world-events.jsonl` that `mode(edge)` per edge matches each story:
#   - mode(HUMAN_EDGE prefix) = Operating  (human-only window)
#   - mode(HUMAN_EDGE)        = Assisted   (human + agent interleaved on one edge)
#   - mode(APP_EDGE)          = Driven     (agent set_value; no human on that edge)
#   - empty window            = Operating
#   - System/neutral-only     = Operating  (Driven NEVER from mere Human-absence)
#
# Computer-use-enabled: with a `cua-driver` on PATH the macOS client is launched so
# the agent's drive lands on a real AX element (agent-cursor overlay / AX checkpoint
# observable out-of-band). Live-LLM credentials come from the repo-root `.env`
# (loaded by the `system` target's `main`); absent them the case SKIPs cleanly.
#
# Run directly:  bash tests/e2e/aarch64-apple-macos/test_modes_projection.sh
# Or via cargo:  cargo test --test e2e -- test_modes_projection
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
cd "$REPO_ROOT"

# --- Toolchain: the macOS app + the C++-linking backend need a full Xcode -----
# Honor an inherited DEVELOPER_DIR / CPLUS_INCLUDE_PATH; otherwise resolve the
# newest installed Xcode and its MacOSX SDK libc++ headers (whisper-rs links C++).
if [ -z "${DEVELOPER_DIR:-}" ]; then
    XCODE_APP="$(ls -d /Applications/Xcode*.app 2>/dev/null | sort -V | tail -1 || true)"
    if [ -n "$XCODE_APP" ]; then
        export DEVELOPER_DIR="$XCODE_APP/Contents/Developer"
    fi
fi
if [ -z "${CPLUS_INCLUDE_PATH:-}" ] && [ -n "${DEVELOPER_DIR:-}" ]; then
    SDK_CXX="$DEVELOPER_DIR/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/usr/include/c++/v1"
    if [ -d "$SDK_CXX" ]; then
        export CPLUS_INCLUDE_PATH="$SDK_CXX"
    fi
fi
echo "DEVELOPER_DIR=${DEVELOPER_DIR:-<unset>}"
echo "CPLUS_INCLUDE_PATH=${CPLUS_INCLUDE_PATH:-<unset>}"

# --- Computer-use gate --------------------------------------------------------
# Default the macOS/cua-driver half ON for the e2e (the deterministic per-edge
# mode assertions still run if it is off / cua-driver is absent).
export RUBBERDUX_SURFACE_DRIVE_MACOS="${RUBBERDUX_SURFACE_DRIVE_MACOS:-1}"
# Select the VC-E.2 modes case inside the shared `system` target's `main`.
export RUBBERDUX_SYSTEM_E2E_CASE=modes

# --- Pre-build the macOS app for the fast direct-exec AX launch ---------------
# `surface_support::launch_macos_surface_client` prefers the pre-built Debug app
# (it inherits the RUBBERDUX_SURFACE_* env LaunchServices would strip). Build it
# once if absent so the agent's drive reaches a registered client within the
# settle window. Skipped when already present to keep the runner fast.
PREBUILT="apps/macos/.build/DerivedData/Build/Products/Debug/Rubberdux (Debug).app/Contents/MacOS/Rubberdux (Debug)"
if [ "$RUBBERDUX_SURFACE_DRIVE_MACOS" = "1" ] && [ ! -f "$PREBUILT" ]; then
    echo "=== Pre-building the macOS Debug app (cargo xtask app build) ==="
    cargo xtask app build
fi

# --- Run the VC-E.2 case ------------------------------------------------------
echo "=== Running VC-E.2 modes projection (cargo test --test system app::surface_modes) ==="
cargo test --test system app::surface_modes -- --nocapture
