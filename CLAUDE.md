# CLAUDE.md

## Build & Run

```bash
cargo check          # type-check
cargo build          # debug build
cargo build --release # release build
cargo test           # run tests
cargo clippy         # lint

# Run (requires TELOXIDE_TOKEN env var)
TELOXIDE_TOKEN=<bot_token> cargo run
```

## Coding Conventions

- Follow the owner's preferred style: convention over configuration, mechanism over strategy, functional over imperative.
- Prefer pure functions and immutable data. Use `&self` over `&mut self` where possible.
- Use `thiserror` for error types. No `unwrap()` or `expect()` in non-test code — propagate errors with `?`.
- Use `log` macros (`log::info!`, `log::error!`) for logging. No `println!` outside of CLI output.
- Derive `serde::Serialize` and `serde::Deserialize` on all data types that cross boundaries (config, API, storage).
- Session data is stored as JSONL (one JSON object per line), not JSON arrays.

## Architecture Rules

- The chat handler must never block. Any work that takes more than a trivial amount of time must be dispatched as a background task.
- Keep the main binary thin — business logic goes in library modules (`src/lib.rs` or `src/` submodules).
- No hardcoded paths, tokens, or secrets. Everything comes from environment variables or config files.

## What NOT To Do

- Do not add features beyond what is asked.
- Do not introduce workflow engines or heavy orchestration frameworks.
- Do not use `unsafe` without explicit approval.
- Do not add dependencies without justification.
