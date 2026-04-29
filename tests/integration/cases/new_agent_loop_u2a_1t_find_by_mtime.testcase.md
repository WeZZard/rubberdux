---
target: agent-loop
---

## Storyline
<!-- The agent should use find to locate recently modified files -->

## User Message
Find all files in the src directory modified in the last 7 days.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains(".rs") || message.text.contains("src")
```
