---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file search by content -->
<!-- The agent should use grep to find files -->

<!-- The agent should report the files containing "error" -->
## User Message
Find all files in the src directory containing the word "error".

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "grep" || t.name == "bash")
```

## Assistant Message
<!-- The assistant should report the files containing "error" -->
