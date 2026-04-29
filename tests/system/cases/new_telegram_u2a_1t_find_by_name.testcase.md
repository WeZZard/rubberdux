---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file find by name -->
<!-- The agent should use bash find to locate the file -->

<!-- The agent should report the matching files -->
## User Message
Find all files named "*.rs" in the src directory using the find command.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should report the matching files -->
```cel
message.text.contains(".rs")
```
