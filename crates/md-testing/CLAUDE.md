# md-testing Crate Guide

## Ordering Directives for Assistant Messages

Test cases use FileCheck-inspired ordering directives to match expected assistant messages against the actual messages produced by the agent. These directives control **how** expected slots are mapped to actual assistant messages, while HTML-comment assertions within each slot control **what** content is verified (via LLM judge).

### `CHECK:` — Gap-Tolerant Sequential Match

**Syntax:** `## CHECK: Assistant Message`

**Semantics:**
- Matches the assistant message somewhere after the previous matched slot.
- Allows gaps: unmatched assistant messages may appear between consecutive matched slots.
- The last expected slot is anchored to the last actual assistant message.
- Any trailing assistant messages after the final expected slot cause the test to fail.

**Example:**
```markdown
## CHECK: Assistant Message
<!-- The assistant should acknowledge the request -->

## CHECK: Assistant Message  
<!-- The assistant should present the final result -->
```

If the agent produces 3 assistant messages (A0, A1, A2) for a 2-slot test:
- Slot 0 → A0
- Slot 1 → A2 (anchored to last message)
- A1 is an allowed gap between the two matched slots.

If the agent produces 4 assistant messages for a 2-slot test:
- Slot 0 → A0
- Slot 1 → A3 (anchored to last message)
- A1 and A2 are allowed gaps.

If the agent produces only 1 assistant message for a 2-slot test:
- **Failure:** `TooFewMessages` — the test expects at least 2 assistant messages.

**Bare heading shorthand:** `## Assistant Message` (without any directive prefix) is treated as implicit `## CHECK: Assistant Message`.

### `CHECK-NEXT:` — Immediate Adjacency (NOT IMPLEMENTED)

**Syntax:** `## CHECK-NEXT: Assistant Message`

**Status:** Explicitly rejected at parse time. Only `CHECK:` is currently supported.

**Rationale:** Our test runner evaluates real LLM agent conversations where tool calls and reasoning steps often inject extra assistant messages between the "logical" user-facing replies. Requiring strict adjacency (`CHECK-NEXT:`) would make tests brittle against these necessary intermediate steps. Use `CHECK:` instead; it tolerates gaps while still enforcing that expected messages appear in the declared order.

**When you need strictness:** If a test genuinely requires that two assistant messages are consecutive with no intervening messages, model that as a content assertion evaluated by the LLM judge (e.g., "The assistant should respond immediately after the previous message without any tool calls in between").

### Matching Algorithm

The `match_assistant_slots` function implements the following rules:

1. **Exact count (1:1 mapping):** When `expected_count == actual_count`, each slot maps directly to the corresponding actual message index.

2. **Single-slot strictness:** When `expected_count == 1`, the test enforces strict 1:1 matching. If the agent produces more than one assistant message, the test fails with `TooManyMessages`. This prevents single-slot tests from accidentally passing when the agent emits unexpected extra messages.

3. **Multi-slot gap tolerance:** When `expected_count > 1` and `actual_count > expected_count`, earlier slots map to their corresponding index, and the final slot is anchored to the last actual message. Extra messages between the second-to-last mapped slot and the final anchored slot are treated as allowed gaps.

4. **Insufficient messages:** When `actual_count < expected_count`, the test fails immediately with `TooFewMessages` before any LLM judge evaluation occurs.

### Unsupported Directives

The parser rejects the following directives with a clear error message:

- `CHECK-NEXT:`
- `CHECK-DAG:`
- `CHECK-NOT:`
- `CHECK-SAME:`
- `CHECK-EMPTY:`
- `CHECK-COUNT-N:`
- `CHECK-LABEL:`

Only `CHECK:` is implemented. Use content assertions (HTML comments evaluated by the LLM judge) to express any requirements that would otherwise need these advanced directives.
