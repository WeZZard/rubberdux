---
target: agent-loop
---

## Storyline
<!-- The agent should use grep with a regex pattern to find matching lines -->

## User Message
Search for all lines starting with "pub fn" in the src directory.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "grep") || message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("pub fn")
```
