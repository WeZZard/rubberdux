---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a grep with regex -->
<!-- The agent should use grep tool with regex pattern -->

<!-- The agent should report matching lines -->
## User Message
Search for all lines starting with "pub fn" in the src directory.

## Tool Call
```cel
message.tool_calls.exists(t, t.name == "grep" || t.name == "bash")
```

## Assistant Message
```cel
message.text.contains("matches") || message.text.contains("Function") || message.text.contains("pub fn")
```
