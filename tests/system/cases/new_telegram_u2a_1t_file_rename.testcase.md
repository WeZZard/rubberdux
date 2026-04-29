---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file rename -->
<!-- The agent should use bash to rename the file -->
<!-- The agent should confirm or report the rename operation result -->

## User Message
Create /tmp/test_rename_old.txt with "old name" and rename it to /tmp/test_rename_new.txt.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should confirm or report the rename operation result -->
