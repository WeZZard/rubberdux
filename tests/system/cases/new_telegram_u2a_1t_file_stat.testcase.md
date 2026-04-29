---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file stat -->
<!-- The agent should use bash stat to get file info -->

<!-- The agent should report the file details -->
## User Message
Get detailed information about Cargo.toml using stat.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should report the file details -->
```cel
message.text.contains("Cargo") || message.text.contains("size") || message.text.contains("bytes")
```
