---
target: agent-loop
---

## Storyline
<!-- The agent should create a file with duplicates and show only unique lines -->

## User Message
Create /tmp/test_unique.txt with "a\nb\na\nc\nb" and show only unique lines.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "write_file")
```

## Assistant Message

```cel
message.text.contains("a") && message.text.contains("b") && message.text.contains("c")
```
