---
target: agent-loop
---

## Storyline
<!-- The agent should show the first 5 lines of a file using bash head -->

## User Message
Use bash head to show the first 5 lines of Cargo.toml.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("Cargo.toml") || message.text.contains("[package]") || message.text.contains("rubberdux")
```
