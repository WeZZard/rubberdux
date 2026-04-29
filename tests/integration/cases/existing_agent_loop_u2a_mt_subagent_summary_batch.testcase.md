---
target: agent-loop
timeout: 240
---

## Storyline
<!-- The agent should address an environment question when multiple user messages are batched -->
<!-- The agent should spawn a subagent for a Google summary when user messages arrive in rapid succession -->
<!-- The agent should report the subagent result back to the user after processing both requests -->

## User Message
Briefly describe the tool execution environment at a high level without running shell commands.

## User Message
Spawn a subagent to write one concise sentence explaining what Google is. The subagent should not use web search or other tools.

## CHECK: Assistant Message

```cel
message.tool_calls.size() == 0 || message.tool_calls.exists(t, t.name == "dispatch_subagent") || message.tool_calls.exists(t, t.name == "agent")
```

<!-- The assistant should address the environment request and dispatch a subagent for the Google summary request -->

## CHECK: Assistant Message

```cel
message.text.contains("Google") || message.text.contains("google") || message.text.contains("search")
```

<!-- The assistant should present the concise Google summary obtained from the subagent -->
