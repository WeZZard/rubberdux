---
target: telegram-channel
timeout: 120
---

## Storyline
<!-- The agent should handle a user asking for a recursive directory copy -->
<!-- The agent should use bash to copy recursively -->

<!-- The agent should confirm or report the copy operation result -->
## User Message
Copy the src directory to /tmp/test_src_copy.

## Tool Call
```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should confirm or report the copy operation result -->
