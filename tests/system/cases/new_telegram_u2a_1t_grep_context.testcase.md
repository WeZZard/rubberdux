---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file grep with context -->
<!-- The agent should use grep with context lines -->

<!-- The agent should report matches with surrounding lines -->
## User Message
Search for "fn main" in the src directory and show 2 lines of context around each match.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "grep" || t.name == "bash")
```

## Assistant Message
```cel
message.text.contains("fn main")
```
