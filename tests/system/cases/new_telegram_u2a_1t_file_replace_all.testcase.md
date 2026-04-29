---
target: telegram-channel
timeout: 120
---

## Storyline
<!-- The agent should handle a user asking for a file replace -->
<!-- The agent should replace text in the file -->
<!-- The agent should confirm the final content -->

## User Message
Create /tmp/test_replace.txt with "hello world hello" and replace all "hello" with "hi".

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("hi")
```
