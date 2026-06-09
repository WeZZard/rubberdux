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
tests/results/
└── YYYYMMDD_HHMMSS/
    ├── integration/
    │   └── <test_name>/
    │       ├── transcript.jsonl     # Raw conversation (Entry objects)
    │       ├── transcript.md        # Markdown narration
    │       └── test.log             # Captured logs
    └── system/
        └── <test_name>/
            ├── execution.json
            ├── evaluation.md
            └── results.json
```

### Artifact verification
- Tests SHOULD assert on collected transcript contents (entry count, roles, etc.)
- On test failure, inspect `transcript.md` to see exactly what the AgentLoop did

### Exemptions
- Unit tests (inline `#[cfg(test)]` mod tests) are exempt — they test isolated functions

## Testcase Evaluator Service

- Testcase runners use `MD_TESTING_LLM_BASE_URL`, `MD_TESTING_LLM_API_KEY`, and `MD_TESTING_LLM_MODEL` for an OpenAI-compatible evaluator service.
- The evaluator service is external-first: start it outside the test runner when possible.
- Set `MD_TESTING_LLM_AUTO_START=true` only when the runner should start the local `mlx_lm.server` fallback itself.

## Testcase Assertion Preference

When writing `*.testcase.md` test cases, **always prefer CEL assertions over LLM judge assertions** (HTML comments). CEL assertions are:
- Deterministic: same input always produces the same result
- Fast: evaluated in microseconds, no LLM call needed
- Debuggable: the expression and its inputs are fully visible in evaluation logs

Use LLM judge assertions only when the check is inherently subjective (tone, style, format quality, steering compliance) and cannot be expressed as a substring/pattern match on `message.text` or a structural check on `message.tool_calls`.

**Guidelines for robust CEL assertions:**
- Use `message.text_lower.contains(...)` for case-insensitive text matching (pre-lowered copy of `message.text`)
- Accept multiple formats for numbers: `"838102050" || "838,102,050"`
- Check tool names by their actual registered name (e.g. `"agent"` for subagent dispatch, not the subagent type)
- For multi-slot tests, remember that the last slot anchors to the last actual message — place `## Tool Call` slots for tool-call assertions and `## Assistant Message` slots for text assertions

## Testcase Section Types

Test cases support the following section types:

### `## System Message`

Optional. Appears between `## Storyline` and the first `## User Message`. Provides the system prompt for the agent conversation. At most one per testcase.

```markdown
## System Message
You are a helpful coding assistant.
```

### `## Tool Call`

Represents an expected assistant turn that contains tool calls. Use CEL assertions to check `message.tool_calls`. Bare `## Tool Call` is implicit `## CHECK: Tool Call`.

````markdown
## Tool Call
```cel
message.tool_calls.exists(t, t.name == "write_file")
```
````

### `## Assistant Message`

Represents an expected assistant turn that contains text content. Use CEL assertions to check `message.text`. Bare `## Assistant Message` is implicit `## CHECK: Assistant Message`.

````markdown
## Assistant Message
```cel
message.text_lower.contains("done")
```
````

### Ordering

All sections share one cursor advancing through actual messages (FileCheck CHECK: semantics). A `## Tool Call` slot matches the next actual message with tool calls. A `## Assistant Message` slot matches the next actual message with text. Gaps between matches are allowed.
