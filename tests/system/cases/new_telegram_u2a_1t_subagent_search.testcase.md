---
target: telegram-channel
timeout: 240
---

## Storyline
<!-- The agent should spawn a subagent when asked to search for files -->
<!-- The agent should report the findings to the user -->

## User Message
Spawn an agent to search for Rust source files in the src directory.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains(".rs") || message.text.contains("Rust") || message.text.contains("source")
```
