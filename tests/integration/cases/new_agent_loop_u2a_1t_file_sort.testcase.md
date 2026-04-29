---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and sort it alphabetically -->

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
