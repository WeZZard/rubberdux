---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file move -->
<!-- The agent should use bash to move the file -->

<!-- The agent should confirm or report the move operation result -->
## User Message
Move /tmp/test_move_src.txt to /tmp/test_move_dst.txt. (Create the source file with "move me" first if needed.)

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should confirm or report the move operation result -->
