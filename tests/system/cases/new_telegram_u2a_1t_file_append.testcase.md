---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file append -->
<!-- The agent should use bash to append to a file -->

## User Message
Create /tmp/test_append.txt with "line1" and append "line2" to it.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("line1") && message.text.contains("line2")
```
