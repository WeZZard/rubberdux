---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a general purpose subagent -->
<!-- The agent should dispatch a general purpose subagent -->

## User Message
Use a general-purpose agent to write a haiku about coding.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "general_purpose")
```

## Assistant Message
<!-- The assistant should present the haiku produced by the subagent to the user -->
