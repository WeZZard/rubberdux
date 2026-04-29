---
target: telegram-channel
timeout: 120
---

## Storyline
<!-- The agent should be able to briefly describe the tool execution environment via Telegram -->
<!-- The agent should be able to spawn a subagent for a Google summary -->
<!-- The agent should report the subagent result back to the user via Telegram -->

## User Message
Briefly describe the tool execution environment at a high level without running shell commands.

## CHECK: Assistant Message
```cel
message.tool_calls.size() == 0
```
<!-- The assistant should briefly describe the tool execution environment and reference the user's environment question -->

## User Message
Spawn a subagent to write one concise sentence explaining what Google is. The subagent should not use web search or other tools.

## CHECK: Assistant Message
```cel
message.tool_calls.size() > 0
```

## CHECK: Assistant Message
```cel
message.text.contains("Google") || message.text.contains("google")
```
