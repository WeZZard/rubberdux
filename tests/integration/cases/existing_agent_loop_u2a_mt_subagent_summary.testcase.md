---
target: agent-loop
timeout: 120
---

## Storyline
<!-- The agent should briefly describe the tool execution environment -->
<!-- The agent should spawn a subagent for a Google summary -->
<!-- The agent should report the subagent result back to the user -->

## User Message
Briefly describe the tool execution environment at a high level without running shell commands.

## CHECK: Assistant Message

```cel
message.tool_calls.size() == 0
```

<!-- The assistant should briefly describe the tool execution environment -->

## User Message
Spawn a subagent to write one concise sentence explaining what Google is. The subagent should not use web search or other tools.

## CHECK: Assistant Message

```cel
message.tool_calls.exists(t, t.name == "dispatch_subagent") || message.tool_calls.exists(t, t.name == "agent")
```

<!-- The assistant should dispatch a subagent for the user's Google summary request -->

## CHECK: Assistant Message

```cel
message.text.contains("Google") || message.text.contains("google") || message.text.contains("search")
```

<!-- The assistant should present the concise Google summary obtained from the subagent -->
