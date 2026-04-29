---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for disk usage -->
<!-- The agent should use bash to check disk usage -->

<!-- The agent should report disk space information -->
## User Message
How much disk space is available?

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should report disk space information -->
