---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and append content to it -->

## User Message
Create /tmp/test_append.txt with "line1" and append "line2" to it.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "write_file")
```

## Assistant Message

```cel
message.text.contains("line1") && message.text.contains("line2")
```
