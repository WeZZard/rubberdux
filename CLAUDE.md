# CLAUDE.md

## Glossary

**Transcript:**

`*.jsonl` file. Ecah line is a user or assistant message.

**Narration:**

`*.jsonl` file message contents formatted in markdown like:

```markdown
## User, YYYY-MM-DD, hh:mm:ss-Tz
<!-- User message contents -->
## Assistant, YYYY-MM-DD, hh:mm:ss-Tz
<!-- Assistant message contents -->
```

## Build & Run

All component builds are orchestrated through the Cargo build system. Never build components in isolation (e.g., `xcodebuild` alone) — use the commands below so dependencies are built in the correct order.

### Components

```bash
# Rust backend (rubberduxd)
cargo build                        # debug build
cargo build --release              # release build
cargo check                        # type-check only
cargo test                         # run tests
cargo clippy                       # lint

# macOS app (builds Rust backend first, then Xcode project)
cargo xtask app build              # debug build: cargo build + xcodebuild
cargo xtask app build --release    # release build
cargo xtask app run                # build + launch the app
cargo xtask app run --release      # release build + launch
cargo xtask app test               # build backend + run the macOS XCTest target

# Full stack (provision VMs, build, launch backend with Telegram)
cargo xtask launch                 # release build + launch rubberduxd --host
cargo xtask stop                   # stop running instance
```

### Git Hooks

After cloning, arm the design-documentation pre-commit linter once:

```bash
cargo xtask install-git-hooks      # points core.hooksPath at .githooks/
```

Any `cargo xtask` command also arms it automatically, so contributors who build are covered without this step. The same linter runs in CI as the authoritative, non-bypassable gate; the local hook is a fast safety net and can be skipped with `git commit --no-verify`.

## macOS VM Concurrency Limits

- Apple Silicon Macs enforce a hard limit of **2 concurrent macOS VMs** via `Virtualization.framework`.
- If a prior test run leaks a VM, the next run that needs a child VM will fail or time out because the slot is already consumed.
- System tests call `cleanup_stale_vms()` before booting VMs, and the harness panics if any `rubberdux-*` VMs are still running after cleanup.
- To run more than 2 VMs concurrently you must boot a **development kernel collection** with `hv_apple_isa_vm_quota` override (requires SIP disable, custom boot policy, and matching KDK from Apple). This is unsupported by Apple and breaks OS updates.
- When running 2 VMs side-by-side, restrict resources per VM (e.g., 8 GB RAM and 6 CPUs) to avoid host contention and IP-timeout failures.

## Coding Conventions

- Follow the owner's preferred style: convention over configuration, mechanism over strategy, functional over imperative.
- Prefer pure functions and immutable data. Use `&self` over `&mut self` where possible.
- Use `thiserror` for error types. No `unwrap()` or `expect()` in non-test code — propagate errors with `?`.
- Use `log` macros (`log::info!`, `log::error!`) for logging. No `println!` outside of CLI output.
- Derive `serde::Serialize` and `serde::Deserialize` on all data types that cross boundaries (config, API, storage).
- Session data is stored as JSONL (one JSON object per line), not JSON arrays.

### Naming Convention

Name identifiers after domain concepts, not implementation details.

**DO:**
- Name environment variables and config keys after what the value represents.
- Name types, functions, and modules after the domain concept they model.

**DO NOT:**
- Leak framework or library names into public-facing identifiers (env vars, config keys, module names).
- Use names that would become misleading if the underlying library were swapped.

**The test**: if the name stops making sense when you replace the library, it needs renaming.

### Path Convention

Paths are domain-first with explicit layer subdirectories. Each directory level answers a question:

- **Level 1** (`src/<domain>/`): *What concept does this belong to?*
- **Level 2** (`src/<domain>/<layer>/`): *What role does it play?*
- **Level 3** (`src/<domain>/<layer>/<name>.rs`): *What specific thing is it?*

Domain core abstractions (traits, types) sit directly in the domain directory, not in a sublayer.

Cross-cutting files that don't belong to a single domain sit at `src/` root.

**The test**: if you can't read the path aloud as a meaningful phrase, the path needs restructuring.

**DO:**
- Name directories after domain concepts.
- Name subdirectories after architectural roles.
- Place domain abstractions (traits, types) directly in the domain directory.
- Place cross-cutting files at `src/` root.

**DO NOT:**
- Name directories after technical concerns (e.g. `utils/`, `helpers/`, `common/`).
- Flatten unrelated files into a single directory.
- Create layer-first structures (e.g. `src/handlers/`, `src/services/`, `src/models/`).

**A valid path IS:**
- Readable as a meaningful phrase (e.g. "the channel adapter for Telegram").
- Self-documenting: the path alone tells you what the file contains.

**A valid path IS NOT:**
- Layer-first with redundant suffixes (e.g. `src/handlers/telegram_handler.rs`).
- A meaningless grouping (e.g. `src/util/helpers.rs`).

**You MUST:**
- Ensure every path reads as a meaningful phrase describing what the file contains.

**You MUST NOT:**
- Create directories that require reading file contents to understand their purpose.

#### Addition Path Rules for Tests

**Unit tests:** inline `#[cfg(test)] mod tests` at the bottom of the source file (Rust convention). Unit tests have direct access to private functions and run with `cargo test --lib`.

**Integration tests:** replicate the path of the tested subject in `src` dir.

<INTEGRATION_TESTS_EXAMPLES>
  src: `src/<domain>/<layer>`
  tests: `tests/integration/<domain>/<layer>_{integration_test_purpose}.rs`
</INTEGRATION_TESTS_EXAMPLES>

<INTEGRATION_TESTS_EXAMPLES>
  src: `src/<domain>`
  tests: `tests/integration/<domain>_{integration_test_purpose}.rs`
</INTEGRATION_TESTS_EXAMPLES>

<INTEGRATION_TESTS_EXAMPLES>
  tests: `tests/integration/{agent-loop|telegram-channel}/test_{testing_purpose}.testcase.md`
</INTEGRATION_TESTS_EXAMPLES>

**System tests:** full-application tests that run on the host machine with real dependencies (e.g., live LLM APIs) but without VM infrastructure or production external services. They exercise the complete application stack natively. Keep them in `tests/system/`.

<SYSTEM_TESTS_EXAMPLES>
  tests: `tests/system/{system_test_purpose}.{rs|sh}`
</SYSTEM_TESTS_EXAMPLES>

<SYSTEM_TESTS_EXAMPLES>
  tests: `tests/system/{agent-loop|telegram-channel}/test_{testing_purpose}.testcase.md`
</SYSTEM_TESTS_EXAMPLES>

**End-to-end tests:** full-application tests that run on the host machine with real dependencies (e.g., live LLM APIs). They exercise the complete application stack natively. Keep them in `tests/e2e/`.

<E2E_TESTS_EXAMPLES>
  tests: `tests/e2e/{<archecture>-<vendor>-<os>[-<environment>]}/{optional: locale}/test_{e2e_life_cycle}_{e2e_test_purpose}.{rs|sh}`
</E2E_TESTS_EXAMPLES>

Explanation to `<archecture>-<vendor>-<os>[-<environment>]`:

- `<architecture>`: Required. The processor architecture. Aligns to the architecture in the LLVM triple. **You MUST use lowercase.**
- `<vendor>`: Required. The OS vendor. Aligns to the OS vendor in the LLVM triple. **You MUST use lowercase.**
- `<os>`: Required. The OS. Aligns to the OS in the LLVM triple. **You MUST use lowercase.**
- `<environemnt>` Optional. The environment of the OS. **You MUST use lowercase.**

`<archecture>-<vendor>-<os>[-<environment>]` basically aligns to the LLVM triple with the optional `environment` fourth element.
You **MUST** use lowercase in each element of the identifier `<archecture>-<vendor>-<os>[-<environment>]`.

Available Environment:

<E2E_ENVIRONMENT>
GNU, GNUABIN32, GNUABI64, GNUEABI, GNUEABIHF, GNUX32, CODE16, EABI, EABIHF, ELFv1, ELFv2, Android, Musl, MuslEABI, MuslEABIHF,
MSVC, Itanium, Cygnus, CoreCLR, Simulator, MacABI
</E2E_ENVIRONMENT>

Available E2E Life-cycle:

<E2E_LIFE_CYCLE>
Build
Profile
Integration
Distribution
Release
</E2E_LIFE_CYCLE>

**Mock data policy:** Only unit tests may use mocked data. Integration tests, system tests, and end-to-end tests must use real model calls.

### Test Case Naming Convention

`.testcase.md` files follow the format:

```
{session_context}_{channel}_{direction}_{turn_type}_{purpose}.testcase.md
```

| Component | Abbreviation | Description |
|-----------|-------------|-------------|
| `session_context` | `new` / `existing` | Fresh session vs. with conversation history |
| `channel` | `agent_loop` / `telegram` | Agent loop only / Telegram channel |
| `direction` | `u2a` / `a2u` | User-to-assistant / Assistant-to-user |
| `turn_type` | `1t` / `mt` | Single-turn / Multi-turn |
| `purpose` | descriptive | What behavior the test verifies |

**Examples:**
- `new_agent_loop_u2a_1t_greeting.testcase.md` — Fresh session, agent loop, user sends greeting, single turn
- `new_telegram_u2a_1t_greeting.testcase.md` — Fresh session, Telegram channel, user sends greeting, single turn
- `existing_agent_loop_u2a_1t_session_continuation.testcase.md` — Existing session, agent loop, user continues conversation

**End-to-end tests:** clarify processor arch (required), vendor (required), os (required), runtime environment (required) and locale (optional).
<example>
  tests: `tests/e2e/{processor_arch}_{vendor}_{os}_{runtime_env}/{optional:locale}/{e2e_test_purpose}.rs`
</example>

### Ordering Directives in Test Cases

Assistant message sections may carry an ordering directive as a prefix on the heading:

```markdown
## CHECK: Assistant Message
<!-- assertion -->
```

- **`CHECK:`** — Matches this assistant message somewhere after the previous match. Gaps (unmatched assistant messages between expected slots) are allowed. The last expected slot is anchored to the last actual assistant message; any trailing assistant messages after the final expected slot cause the test to fail.
- **Bare `## Assistant Message`** — Shorthand for `## CHECK: Assistant Message` (implicit CHECK).

Only `CHECK:` is implemented. Unsupported directives (e.g. `CHECK-NEXT:`, `CHECK-DAG:`, `CHECK-NOT:`) are rejected at parse time.

### Front Matter: `timeout`

Each `.testcase.md` may specify a per-turn timeout in seconds:

```yaml
---
timeout: 60
---
```

Default is 60 seconds. Override when the test involves slow operations (tool calls, subagent dispatch, web search).

### Comment Scoping Rule

Comments explain the purpose of the item they're attached to, not how other parts of the system work.

**DO:**
- Explain WHAT the item represents and WHY it exists.
- Reference other modules when it explains this item's purpose.
- Reference the design document that governs this item (e.g. `// see docs/<domain>/…`) so a reader can find the specification it implements.
- Use doc comments (`///`) on public items to describe the item's contract.
- Use inline comments (`//`) to explain non-obvious *why* for the adjacent line/block.

**DO NOT:**
- Explain HOW another module works in this item's comment.
- Restate what the code does (e.g. `// increment counter` above `counter += 1`).
- Write comments that would need updating when unrelated code changes.

**A scoped comment IS:**
- A comment that explains this item's origin and purpose, possibly referencing but not explaining other modules.
- A comment that explains a non-obvious *why* for the adjacent code.

**A scoped comment IS NOT:**
- A comment that explains another module's internals at this item's location.
- A comment that restates the code in natural language.

**You MUST:**
- Scope every comment to the item it's attached to.
- Apply the maintenance test: if the comment would need updating when unrelated code changes, it's out of scope.

**You MUST NOT:**
- Use comments to document how other parts of the system work — that belongs in those parts' own comments.
- Write comments that create implicit coupling between unrelated modules.

## Design Documentation

Design documentation is the system- and subsystem-level record of *why* the project is shaped the way it is: context, architecture, the model, decisions and their rejected alternatives, and design language. It lives in dedicated documents — not in code comments (which serve the narrower, item-scoped purpose of the Comment Scoping Rule) and not in `CLAUDE.md` files (which hold only the principles for manipulating their folder). This mirrors the established split between Architecture Decision Records and inline comments.

### Where Design Documents Live

Design documents live under `docs/`, organized to mirror the code's domain-first paths:

- Code at `src/<domain>/<layer>/` → docs at `docs/<domain>/<layer>/`.
- Code under `apps/<platform>/` → docs under `docs/apps/`.
- Cross-cutting design that belongs to no single code path sits at the nearest shared parent under `docs/`.

**The test**: the doc path is the code path with its root (`src/`, or the repo root for `apps/`) rebased onto `docs/`. If you can compute one from the other, discovery is working.

### Finding a Subsystem's Design Documents

Compute the doc location from the code location by the path-mirror rule above — that mapping is the sole discovery mechanism. There are no hand-maintained pointers in `CLAUDE.md` files to drift out of sync; the convention is enforced mechanically by the design-documentation linter (`cargo xtask lint`). It runs as a git pre-commit hook — the tracked `.githooks/pre-commit`, armed via `cargo xtask install-git-hooks` (which points `core.hooksPath` at it; also auto-armed by any `cargo xtask` run) — and as a CI job, which is the authoritative, non-bypassable gate.

**You MUST:**
- Place new design documentation under the mirrored `docs/` path so the linter can find it.

**You MUST NOT:**
- Put design documentation — architecture, rationale, rejected alternatives — into `CLAUDE.md` files or code comments. Those reference the design documents; they do not contain them.
- Add hand-maintained design-document pointers to a folder's `CLAUDE.md`; rely on the path-mirror convention instead.

## Testing

- You **MUST** make all the testing tasks in each programming langauge (Rust, TypeScript, Swift, Objective-C) this project orchestrated by Rust cargo.
- You **MUST** not set timeout when execute testing.

## User Experience Rules

- The chat handler must never block. Any work that takes more than a trivial amount of time must be dispatched as a background task.

## Architecture Rules

- Keep the main binary thin — business logic goes in library modules (`src/lib.rs` or `src/` submodules).
- No hardcoded paths, tokens, or secrets. Everything comes from environment variables or config files.

## What NOT To Do

- Do not add features beyond what is asked.
- Do not introduce workflow engines or heavy orchestration frameworks.
- Do not use `unsafe` without explicit approval.
- Do not add dependencies without justification.

## Grand Plan

You **MUST** save grand plan to `.plans` when you are asked to save/consolidate a grand plan.

<GRAND_PLAN_LOCATION_EXAMPLE>
.plans/{YYYY-MM-DD-hh-mm-ss-Tz}-{plan-name}/index.md
.plans/{YYYY-MM-DD-hh-mm-ss-Tz}-{plan-name}/chapter-{chapter_number}-{chapter-name}.md
</GRAND_PLAN_LOCATION_EXAMPLE>

## OpenCode

You **MUST** save plan to `.opencode/plans/{YYYY-MM-DD-hh-mm-ss-Tz}-{plan-name}`.md when you (OpenCode) are asked to save/consolidate the plan.
You **MUST** use `{YYYY-MM-DD-hh-mm-ss-Tz}` has the prefix of the plan file.
You **MUST** use lower case in the plan file name.
