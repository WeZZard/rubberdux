#!/bin/bash
# VC-E.3 — the HEADLINE acceptance: a recorded mixed Operating/Assisted/Driven
# session (with the interleaved SurfaceObserved stream) replays to a BYTE-IDENTICAL
# World with ZERO model/tool re-invocation and IDENTICAL per-edge mode. (Inv 6, 18, 19.)
#
# End-to-end runner: boots the SAME proven live stack as V-system-drive (in-process
# host + a real `rubberduxd --agent` worker + SurfaceRouter + the macOS surface
# client) via the `system` target's `app::surface_mixed_replay` case, records a live
# mixed session against the REAL model — a benign first turn (Operating: human alone)
# and one eliciting `set_value` turn (Assisted on the human edge; Driven on the app
# edge) with the macOS reporter's SurfaceObserved perceptions interleaved — to a real
# `world-events.jsonl`, then REPLAYS that recorded log under the replay driver
# (`drive_replay` + `ReplayCursor`, an `ExplodingClient` at 0 calls) and asserts:
#   - byte-identical World (live fold vs replay; canonical-serialize digest match)
#   - zero model/tool re-invocation (exploding client stays at 0)
#   - identical per-edge mode live-vs-replay:
#       mode(HUMAN_EDGE prefix) = Operating, mode(HUMAN_EDGE) = Assisted,
#       mode(APP_EDGE) = Driven
#
# Computer-use-enabled: with a `cua-driver` on PATH the macOS client is launched so
# the agent's drive lands on a real AX element (agent-cursor overlay observable
# out-of-band) and the live SurfaceObserved stream is interleaved into the recording.
# Live-LLM credentials come from the repo-root `.env` (loaded by the `system`
# target's `main`); absent them the case SKIPs cleanly.
#
# Run directly:  bash tests/e2e/aarch64-apple-macos/test_mixed_mode_replay.sh
# Or via cargo:  cargo test --test e2e -- test_mixed_mode_replay
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
# Default the macOS/cua-driver half ON for the e2e (the deterministic record→replay
# assertions still run if it is off / cua-driver is absent — minus the live
# SurfaceObserved stream).
export RUBBERDUX_SURFACE_DRIVE_MACOS="${RUBBERDUX_SURFACE_DRIVE_MACOS:-1}"
# Select the VC-E.3 record→replay case inside the shared `system` target's `main`.
export RUBBERDUX_SYSTEM_E2E_CASE=mixed_replay

# --- Pre-build the macOS app for the fast direct-exec AX launch ---------------
# `surface_support::launch_macos_surface_client` prefers the pre-built Debug app
# (it inherits the RUBBERDUX_SURFACE_* env LaunchServices would strip). Build it
# once if absent so the agent's drive reaches a registered client within the settle
# window. Skipped when already present to keep the runner fast.
PREBUILT="apps/macos/.build/DerivedData/Build/Products/Debug/Rubberdux (Debug).app/Contents/MacOS/Rubberdux (Debug)"
if [ "$RUBBERDUX_SURFACE_DRIVE_MACOS" = "1" ] && [ ! -f "$PREBUILT" ]; then
    echo "=== Pre-building the macOS Debug app (cargo xtask app build) ==="
    cargo xtask app build
fi

# --- Run the VC-E.3 case ------------------------------------------------------
echo "=== Running VC-E.3 mixed-mode record→replay (cargo test --test system app::surface_mixed_replay) ==="
cargo test --test system app::surface_mixed_replay -- --nocapture
