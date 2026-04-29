---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file sort -->
<!-- The agent should use bash to sort the file -->

<!-- The agent should report or confirm the sorted content -->
## User Message
Create /tmp/test_sort.txt with "zebra\napple\nmango" and sort it alphabetically.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("apple") && message.text.contains("mango") && message.text.contains("zebra")
```
