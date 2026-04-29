---
target: agent-loop
---

## Storyline
<!-- The agent should search for a pattern with context lines around each match -->

## User Message
Search for "fn main" in the src directory and show 2 lines of context around each match.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "grep") || message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("fn main")
```
