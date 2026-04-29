---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a glob with multiple patterns -->
<!-- The agent should use glob tool with multiple patterns -->

<!-- The agent should report the matching files -->
## User Message
Find all .rs and .toml files in the project.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains(".rs") && message.text.contains(".toml")
```
