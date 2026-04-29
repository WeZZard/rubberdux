---
target: agent-loop
---

## Storyline
<!-- The agent should use find to locate files by name pattern -->

## User Message
Find all files named "*.rs" in the src directory using the find command.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains(".rs")
```
