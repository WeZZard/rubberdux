---
target: agent-loop
---

## Storyline
<!-- The agent should use glob to find files matching multiple patterns -->

## User Message
Find all .rs and .toml files in the project.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "glob") || message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains(".rs") && message.text.contains(".toml")
```
