---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a symlink creation -->
<!-- The agent should use bash to create a symlink -->

<!-- The agent should confirm or report the symlink creation -->
## User Message
Create a symlink /tmp/test_link.txt pointing to /tmp/test_target.txt with content "target".

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should confirm or report the symlink creation -->
```cel
message.text.contains("symlink") || message.text.contains("link") || message.text.contains("test_link")
```
