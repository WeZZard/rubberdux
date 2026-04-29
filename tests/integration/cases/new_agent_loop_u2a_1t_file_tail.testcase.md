---
target: agent-loop
---

## Storyline
<!-- The agent should show the last 5 lines of a file -->

## User Message
Show the last 5 lines of Cargo.toml.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "read_file")
```

## Assistant Message
<!-- The assistant should show the last 5 lines of Cargo.toml -->
