---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file comparison -->
<!-- The agent should use a tool to compare files -->

<!-- The agent should compare the files and report differences -->
## User Message
Create two files /tmp/test_cmp_a.txt with "hello" and /tmp/test_cmp_b.txt with "world", then compare them.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should compare the files and report differences -->
```cel
message.text.contains("hello") || message.text.contains("world") || message.text.contains("differ")
```
