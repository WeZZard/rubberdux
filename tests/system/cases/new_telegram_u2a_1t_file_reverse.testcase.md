---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file reverse -->
<!-- The agent should use bash to reverse the file -->

<!-- The agent should report or confirm the reversed content -->
## User Message
Create /tmp/test_reverse.txt with "line1\nline2\nline3" and reverse the line order.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should report or confirm the reversed content -->
```cel
message.text.contains("line3") || message.text.contains("line1")
```
