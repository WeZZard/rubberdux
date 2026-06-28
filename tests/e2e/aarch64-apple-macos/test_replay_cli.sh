#!/bin/bash
# VC-5.1 — the CLI end-to-end: drive EVERY `cargo xtask replay` subcommand
# against a real on-disk session and assert its observable effect.
#
# This is the SINK for CR-sink-cli. The OFFLINE subcommands
# (`list` / `restore` / `fork` / `diff` / `prune` / `drop` / `pin` / `unpin`) are
# exercised deterministically and credential-free; their effects are asserted via
# exit codes + output greps. The LIVE `run` (replay → live, PAID) is exercised
# ONLY under the `RUBBERDUX_LLM_*` gate and skips cleanly when absent.
#
# ## Session bootstrap approach (and why)
# We SYNTHESIZE a minimal valid `world-events.jsonl` fixture in an isolated temp
# app-home rather than recording one live. The fixture is a `SessionStarted` +
# `UserMessage` prefix — the exact `Event` / `LogicalInput` JSON shapes the
# integration tests in `tests/integration/agent/world_*.rs` build and the
# `#[serde(tag = "kind", rename_all = "snake_case")]` encoding declared in
# `src/agent/world/inputs.rs`. The offline subcommands only need a recorded prefix
# that folds faithfully (`restore`/`diff`/`prune` fold it; `fork` clones a verbatim
# prefix + one exogenous edit), so a two-event log is a legitimate recorded session
# (a crash before the model replied) and keeps the offline half hermetic — no model
# call, no network, no developer secrets. The live `run` then forks a FRESH branch
# whose edit has no recorded model result, so its first `CallModel` diverges off the
# empty cursor, flips Replay→Live exactly once, and makes one real paid call.
#
# ## Isolation
# The whole run is rooted at a private temp `RUBBERDUX_HOME` (the same env var
# `rubberdux::session::SessionManager` resolves), removed by an EXIT trap, so the
# test NEVER reads or writes the developer's real `~/.rubberdux` sessions.
#
# ## How to invoke
#   bash tests/e2e/aarch64-apple-macos/test_replay_cli.sh
# The xtask build links C++ (whisper-rs), so if it is not already on your env,
# prefix with the SDK libc++ headers:
#   CPLUS_INCLUDE_PATH="$(xcrun --show-sdk-path)/usr/include/c++/v1" \
#     bash tests/e2e/aarch64-apple-macos/test_replay_cli.sh
#
# `cargo xtask` is the alias `cargo run --manifest-path ./xtask/Cargo.toml --`
# (see `.cargo/config.toml`); below it is expanded literally with `--quiet` added
# so cargo's progress noise stays off stdout and the program's own `println!`
# output is what the assertions grep.
#
# See docs/agent/world/ecs-runtime.md and the plan's Verification §5 VC-5.1.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
cd "$REPO_ROOT"

# ---------------------------------------------------------------------------
# Toolchain — the agent-feature backend links C++ (whisper-rs), so resolve a
# full Xcode + the MacOSX SDK libc++ headers unless they are already inherited.
# ---------------------------------------------------------------------------
if [ -z "${DEVELOPER_DIR:-}" ]; then
    XCODE_APP="$(ls -d /Applications/Xcode*.app 2>/dev/null | sort -V | tail -1 || true)"
    if [ -n "$XCODE_APP" ]; then
        export DEVELOPER_DIR="$XCODE_APP/Contents/Developer"
    fi
fi
if [ -z "${CPLUS_INCLUDE_PATH:-}" ]; then
    SDK_PATH="$(xcrun --show-sdk-path 2>/dev/null || true)"
    if [ -n "$SDK_PATH" ] && [ -d "$SDK_PATH/usr/include/c++/v1" ]; then
        export CPLUS_INCLUDE_PATH="$SDK_PATH/usr/include/c++/v1"
    fi
fi
echo "DEVELOPER_DIR=${DEVELOPER_DIR:-<unset>}"
echo "CPLUS_INCLUDE_PATH=${CPLUS_INCLUDE_PATH:-<unset>}"

# ---------------------------------------------------------------------------
# Isolated app-home — never touch the developer's real ~/.rubberdux.
# ---------------------------------------------------------------------------
RUBBERDUX_HOME="$(mktemp -d "${TMPDIR:-/tmp}/rubberdux-replay-cli.XXXXXX")"
export RUBBERDUX_HOME
cleanup() { rm -rf "$RUBBERDUX_HOME"; }
trap cleanup EXIT
echo "RUBBERDUX_HOME=$RUBBERDUX_HOME (isolated; removed on exit)"

# `cargo xtask replay …`, expanded from the .cargo alias, quietened so stdout is
# the program's own output alone.
XTASK=(cargo run --quiet --manifest-path "$REPO_ROOT/xtask/Cargo.toml" --)

# ---------------------------------------------------------------------------
# Assertion helpers — every failure prints a self-explaining message and the
# captured output, then exits non-zero (set -e backs the whole script).
# ---------------------------------------------------------------------------
fail() {
    echo "FAIL: $*" >&2
    exit 1
}

assert_contains() { # haystack needle message
    case "$1" in
        *"$2"*) ;;
        *) fail "$3 (expected to contain: '$2') -- got: <<<$1>>>" ;;
    esac
}

assert_match() { # string extended-regex message
    printf '%s' "$1" | grep -Eq "$2" \
        || fail "$3 (expected to match: /$2/) -- got: <<<$1>>>"
}

# ---------------------------------------------------------------------------
# Synthesize the session fixture — a minimal valid recorded prefix.
# ---------------------------------------------------------------------------
SID="2026-06-28-00-00-00-UTC"
SESSION_DIR="$RUBBERDUX_HOME/sessions/$SID"
mkdir -p "$SESSION_DIR"
cat > "$SESSION_DIR/world-events.jsonl" <<'JSONL'
{"origin":"system","edge":0,"at":0,"wall":null,"input":{"kind":"session_started","seed":7}}
{"origin":"human","edge":0,"at":1,"wall":null,"input":{"kind":"user_message","to":0,"text":"first question"}}
JSONL
# A `latest` symlink so a `--session`-less prune could resolve it too (and so the
# `list` annotation matches the production layout). Best-effort.
ln -snf "$SESSION_DIR" "$RUBBERDUX_HOME/latest" 2>/dev/null || true
echo "Synthesized session $SID with a 2-event world-events.jsonl"

# ---------------------------------------------------------------------------
# Pre-build the xtask binary once so each subcommand run below is fast.
# ---------------------------------------------------------------------------
echo "=== Building xtask (cargo build --manifest-path xtask/Cargo.toml) ==="
cargo build --quiet --manifest-path "$REPO_ROOT/xtask/Cargo.toml"

# ===========================================================================
# OFFLINE subcommands
# ===========================================================================

# --- list (initial) — the session is present, no branches yet ---------------
echo "=== replay list (initial) ==="
out=$("${XTASK[@]}" replay list --session "$SID" 2>&1) \
    || fail "replay list exited non-zero: $out"
echo "$out"
assert_contains "$out" "session $SID" "list must name the session"
assert_contains "$out" "branches: 0" "list must show zero branches before any fork"
echo "  ok: list names the session with zero branches"

# --- restore — reconstruct to a tick and print tick + 64-hex World digest ----
echo "=== replay restore ==="
out=$("${XTASK[@]}" replay restore --session "$SID" 2>&1) \
    || fail "replay restore exited non-zero: $out"
echo "$out"
assert_match "$out" "restored session $SID to tick [0-9]+ \(digest [0-9a-f]{64}\)" \
    "restore must print the restored tick and a 64-hex World digest"
echo "  ok: restore printed tick + digest"

# --- fork (accept) — an exogenous edit yields a deterministic 64-hex BranchId -
echo "=== replay fork (exogenous user: edit, accepted) ==="
B1=$("${XTASK[@]}" replay fork --session "$SID" --at 1 --edit "user:edited-one" 2>/dev/null)
echo "  B1=$B1"
assert_match "$B1" "^[0-9a-f]{64}$" "fork must print a 64-hex BranchId on stdout"
# A second, distinct edit yields a distinct branch (content-addressed identity).
B2=$("${XTASK[@]}" replay fork --session "$SID" --at 1 --edit "user:edited-two" 2>/dev/null)
echo "  B2=$B2"
assert_match "$B2" "^[0-9a-f]{64}$" "fork must print a 64-hex BranchId for the second edit"
[ "$B1" != "$B2" ] || fail "distinct edits must produce distinct BranchIds (got $B1 == $B2)"
echo "  ok: two distinct forks produced two distinct 64-hex BranchIds"

# --- fork (reject) — a non-exogenous (derived tool:) edit exits non-zero -----
echo "=== replay fork (derived tool: edit, rejected) ==="
if out=$("${XTASK[@]}" replay fork --session "$SID" --at 1 --edit "tool:should-reject" 2>&1); then
    fail "fork with a derived (tool:) edit must be rejected with a non-zero exit, but it succeeded: $out"
fi
echo "  ok: non-exogenous --edit rejected with a non-zero exit"

# --- list (after forks) — two branch directories are now present ------------
echo "=== replay list (after forks) ==="
out=$("${XTASK[@]}" replay list --session "$SID" 2>&1) \
    || fail "replay list (after forks) exited non-zero: $out"
echo "$out"
assert_contains "$out" "branches: 2" "list must show two branches after two forks"
echo "  ok: list shows two branches"

# --- diff (branch vs main) — divergence tick printed ------------------------
echo "=== replay diff main vs B1 (divergent) ==="
out=$("${XTASK[@]}" replay diff --session "$SID" main "$B1" 2>&1) \
    || fail "replay diff main $B1 exited non-zero: $out"
echo "$out"
assert_match "$out" "diverged at tick [0-9]+" \
    "an edited branch must diverge from main at a tick"
echo "  ok: diff reports a divergence tick"

# --- diff (main vs main) — identical ----------------------------------------
echo "=== replay diff main vs main (identical) ==="
out=$("${XTASK[@]}" replay diff --session "$SID" main main 2>&1) \
    || fail "replay diff main main exited non-zero: $out"
echo "$out"
assert_contains "$out" "identical" "main vs main must report identical"
echo "  ok: diff reports identical for main vs main"

# --- prune (initial) — counts printed; nothing dropped yet ------------------
echo "=== replay prune (initial, --keep 3) ==="
out=$("${XTASK[@]}" replay prune --session "$SID" --keep 3 2>&1) \
    || fail "replay prune exited non-zero: $out"
echo "$out"
assert_match "$out" "[0-9]+ evicted, [0-9]+ tiered, [0-9]+ reclaimed, [0-9]+ protected" \
    "prune must print evicted/tiered/reclaimed/protected counts"
assert_contains "$out" "0 reclaimed, 2 protected" \
    "no branch is dropped yet, so both branches are protected and none reclaimed"
echo "  ok: prune printed the four counts (0 reclaimed, 2 protected)"

# --- pin / unpin — reachability-root toggles --------------------------------
echo "=== replay pin / unpin B2 ==="
out=$("${XTASK[@]}" replay pin --session "$SID" "$B2" 2>&1) \
    || fail "replay pin exited non-zero: $out"
assert_contains "$out" "pinned branch $B2" "pin must confirm it pinned the branch"
out=$("${XTASK[@]}" replay unpin --session "$SID" "$B2" 2>&1) \
    || fail "replay unpin exited non-zero: $out"
assert_contains "$out" "unpinned branch $B2" "unpin must confirm it unpinned the branch"
# Re-pin B2 so the destructive prune below proves the pin PROTECTS it.
out=$("${XTASK[@]}" replay pin --session "$SID" "$B2" 2>&1) \
    || fail "replay re-pin exited non-zero: $out"
assert_contains "$out" "pinned branch $B2" "re-pin must confirm it pinned the branch"
echo "  ok: pin / unpin / re-pin toggled B2"

# --- drop — tombstone both branches; the dir survives until prune -----------
echo "=== replay drop B1 and B2 ==="
out=$("${XTASK[@]}" replay drop --session "$SID" "$B1" 2>&1) \
    || fail "replay drop B1 exited non-zero: $out"
assert_contains "$out" "dropped branch $B1" "drop must confirm it dropped B1"
[ -d "$SESSION_DIR/branches/$B1" ] \
    || fail "drop must only tombstone — B1's branch directory must still exist before prune"
out=$("${XTASK[@]}" replay drop --session "$SID" "$B2" 2>&1) \
    || fail "replay drop B2 exited non-zero: $out"
assert_contains "$out" "dropped branch $B2" "drop must confirm it dropped B2"
echo "  ok: both branches tombstoned; B1 dir still present pre-prune"

# --- prune (destructive) — dropped+unpinned reclaimed; pinned protected -----
echo "=== replay prune (after drops) ==="
out=$("${XTASK[@]}" replay prune --session "$SID" 2>&1) \
    || fail "replay prune (after drops) exited non-zero: $out"
echo "$out"
assert_contains "$out" "1 reclaimed, 1 protected" \
    "the dropped+unpinned B1 is reclaimed; the dropped-but-pinned B2 is protected"
[ ! -d "$SESSION_DIR/branches/$B1" ] \
    || fail "prune must reclaim the dropped+unpinned branch — B1's directory should be gone"
[ -d "$SESSION_DIR/branches/$B2" ] \
    || fail "a pinned branch must survive prune — B2's directory should still exist"
echo "  ok: drop+prune removed B1's dir; pin protected B2's dir"

echo
echo "OFFLINE replay CLI subcommands: ALL ASSERTIONS PASSED"
echo

# ===========================================================================
# LIVE `run` (replay → live, PAID) — gated on RUBBERDUX_LLM_*
# ===========================================================================

# Load RUBBERDUX_LLM_* from the repo .env when the env does not already carry
# them (the replay CLI does NOT auto-load .env). Only RUBBERDUX_LLM_* keys, and
# only when currently unset, so an explicit environment always wins.
load_llm_env_from_dotenv() {
    local envfile="$1"
    [ -f "$envfile" ] || return 0
    while IFS= read -r line; do
        line="${line#"${line%%[![:space:]]*}"}"  # strip leading whitespace
        case "$line" in
            RUBBERDUX_LLM_*=*)
                local key="${line%%=*}"
                local val="${line#*=}"
                val="${val%\"}"; val="${val#\"}"   # strip surrounding "double" quotes
                val="${val%\'}"; val="${val#\'}"   # strip surrounding 'single' quotes
                if [ -z "${!key:-}" ]; then
                    export "$key=$val"
                fi
                ;;
        esac
    done < "$envfile"
}
load_llm_env_from_dotenv "$REPO_ROOT/.env"

if [ -z "${RUBBERDUX_LLM_API_KEY:-}" ]; then
    echo "SKIP live 'replay run': RUBBERDUX_LLM_API_KEY absent (and not in .env)."
    echo "    Set RUBBERDUX_LLM_API_KEY (and optionally RUBBERDUX_LLM_BASE_URL /"
    echo "    RUBBERDUX_LLM_MODEL) to exercise the paid replay→live 'run' path."
    echo "ALL ASSERTIONS PASSED (offline); live run skipped cleanly."
    exit 0
fi

echo "=== replay run (LIVE, PAID — RUBBERDUX_LLM_* present) ==="
# A fresh branch whose edit (a new user turn) has no recorded model result, so its
# first CallModel diverges off the empty cursor, flips Replay→Live exactly once,
# and makes a single real paid model call.
BLIVE=$("${XTASK[@]}" replay fork --session "$SID" --at 1 \
    --edit "user:Reply with exactly one word: ping" 2>/dev/null)
echo "  BLIVE=$BLIVE"
assert_match "$BLIVE" "^[0-9a-f]{64}$" "the live-run fork must yield a 64-hex BranchId"

out=$("${XTASK[@]}" replay run --session "$SID" "$BLIVE" 2>&1) \
    || fail "live 'replay run' failed (paid replay→live path): $out"
echo "$out"
assert_match "$out" "ran branch $BLIVE to quiescence: tick [0-9]+ \(digest [0-9a-f]{64}\)" \
    "live run must drive the branch to quiescence and print its tick + digest"
echo "  ok: live replay→live run reached quiescence"

echo
echo "ALL ASSERTIONS PASSED (offline + live run)"
