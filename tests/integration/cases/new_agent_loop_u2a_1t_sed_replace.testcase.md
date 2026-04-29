---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and use sed to replace text in it -->

## User Message
Create /tmp/test_sed.txt with "hello world" and use sed to replace "world" with "rubberdux".

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "write_file")
```

## Assistant Message

```cel
message.text.contains("rubberdux")
```
