---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking to create a nested directory structure -->
<!-- The agent should use bash to create directories -->

<!-- The agent should confirm or report the directory creation -->
## User Message
Create the directory /tmp/test_nested/a/b/c.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should confirm or report the directory creation -->
