---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file head -->
<!-- The agent should use a tool to read the file -->

## User Message
Show the first 5 lines of Cargo.toml.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should show the first 5 lines of Cargo.toml -->
```cel
message.text.contains("Cargo") || message.text.contains("package") || message.text.contains("toml")
```
