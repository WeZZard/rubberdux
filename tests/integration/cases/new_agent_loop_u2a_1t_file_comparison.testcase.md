---
target: agent-loop
---

## Storyline
<!-- The agent should create two files and compare them using diff -->

## User Message
Create two files /tmp/test_cmp_a.txt with "hello" and /tmp/test_cmp_b.txt with "world", then compare them.

## Assistant Message

```cel
message.tool_calls.size() > 0
```

## Assistant Message

```cel
message.text.contains("hello") || message.text.contains("world") || message.text.contains("differ")
```
