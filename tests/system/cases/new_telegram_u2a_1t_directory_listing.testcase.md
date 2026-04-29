---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking to list a directory structure -->
<!-- The agent should use glob or bash to list files -->

## User Message
List all files in the src directory.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should list files in the src directory -->
```cel
message.text.contains("src") || message.text.contains(".rs")
```
