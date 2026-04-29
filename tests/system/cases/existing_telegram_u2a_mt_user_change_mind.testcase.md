---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a multi-turn conversation where the user changes their mind -->
<!-- The agent should adapt to the new request without confusion -->

## User Message
Write "hello" to /tmp/test_change.txt.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## User Message
Actually, write "goodbye" to /tmp/test_change.txt instead.

## Assistant Message
<!-- The assistant should handle the updated request appropriately -->
```cel
message.text.contains("goodbye") || message.text.contains("updated") || message.text.contains("changed") || message.text.contains("written")
```
