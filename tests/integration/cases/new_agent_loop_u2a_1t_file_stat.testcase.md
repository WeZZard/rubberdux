---
target: agent-loop
---

## Storyline
<!-- The agent should use stat to get detailed file information -->

## User Message
Get detailed information about Cargo.toml using stat.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("Cargo.toml") || message.text.contains("size") || message.text.contains("modif")
```
