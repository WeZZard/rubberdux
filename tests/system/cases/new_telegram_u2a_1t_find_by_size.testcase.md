---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file find by size -->
<!-- The agent should use bash find to locate large files -->

<!-- The agent should report the matching files -->
## User Message
Find all files in the src directory larger than 1KB.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should report the matching files -->
