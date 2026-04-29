---
target: agent-loop
---

## Storyline
<!-- The agent should check and report disk space information -->

## User Message
How much disk space is available?

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should report disk space information -->
