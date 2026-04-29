---
target: agent-loop
---

## Storyline
<!-- The agent should dispatch a general-purpose subagent to complete a creative task -->

## User Message
Use a general-purpose agent to write a haiku about coding.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "dispatch_subagent") || message.tool_calls.exists(t, t.name == "agent")
```

## Assistant Message
<!-- The assistant should present the haiku produced by the subagent to the user -->
