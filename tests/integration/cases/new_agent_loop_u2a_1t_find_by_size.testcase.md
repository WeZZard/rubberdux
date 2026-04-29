---
target: agent-loop
---

## Storyline
<!-- The agent should use find to locate files larger than a given size -->

## User Message
Find all files in the src directory larger than 1KB.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should report the matching files -->
