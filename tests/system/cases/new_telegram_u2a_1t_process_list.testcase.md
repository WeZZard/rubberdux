---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a process list -->
<!-- The agent should use bash to list processes -->

## User Message
List the current running processes.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should list running processes for the user -->
