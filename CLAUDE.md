# CLAUDE.md

## Build & Run

```bash
cargo check          # type-check
cargo build          # debug build
cargo build --release # release build
cargo test           # run tests
cargo clippy         # lint

# Run (requires TELEGRAM_BOT_TOKEN env var)
TELEGRAM_BOT_TOKEN=<bot_token> cargo run
```

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

### Comment Scoping Rule

Comments explain the purpose of the item they're attached to, not how other parts of the system work.

**DO:**
- Explain WHAT the item represents and WHY it exists.
- Reference other modules when it explains this item's purpose.
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

## Architecture Rules

- The chat handler must never block. Any work that takes more than a trivial amount of time must be dispatched as a background task.
- Keep the main binary thin — business logic goes in library modules (`src/lib.rs` or `src/` submodules).
- No hardcoded paths, tokens, or secrets. Everything comes from environment variables or config files.

## What NOT To Do

- Do not add features beyond what is asked.
- Do not introduce workflow engines or heavy orchestration frameworks.
- Do not use `unsafe` without explicit approval.
- Do not add dependencies without justification.
