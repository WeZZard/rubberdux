# Testing Rules

## Transcript Collection

For all tests that exercise the AgentLoop (integration, system, e2e), transcripts MUST be collected and made available for debugging.

### What to collect
- **Transcript:** All Entry objects (User, Assistant, Tool messages) that pass through the AgentLoop, persisted as JSONL
- **Narration:** Markdown rendering of the transcript for human readability
- **Logs:** `log::info!`, `log::error!`, etc. emitted during the test (best effort — only the first test per run captures logs due to logger initialization constraints)

### How to collect
- Pass a `session_path` to `AgentLoopConfig` to exercise file persistence
- Use `tests::support::artifact::artifact_dir(test_name)` to create per-test directories
- Call `tests::support::log_capture::init(&log_path)` to capture logs
- After the test, call `artifact::narrate_session(&session_path)` and `artifact::write_narration(...)`

### Where to store
```
test_results/
└── YYYYMMDD_HHMMSS-integration/
    └── <test_name>/
        ├── transcript.jsonl     # Raw conversation (Entry objects)
        ├── transcript.md        # Markdown narration
        └── test.log             # Captured logs
```

### Artifact verification
- Tests SHOULD assert on collected transcript contents (entry count, roles, etc.)
- On test failure, inspect `transcript.md` to see exactly what the AgentLoop did

### Exemptions
- Unit tests (inline `#[cfg(test)]` mod tests) are exempt — they test isolated functions
