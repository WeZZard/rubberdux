---
target: telegram-channel
timeout: 240
---

## Storyline
<!-- The agent should handle a user asking for a plan subagent -->
<!-- The agent should dispatch a plan subagent -->

## User Message
Use a plan subagent to propose three concise steps for adding a tiny `/health` REST endpoint in Rust, then report the three steps.

## Tool Call
```cel
message.tool_calls.exists(t, t.name == "agent")
```

## Assistant Message
<!-- The assistant should present the concise three-step plan produced by the subagent to the user -->
```cel
message.text.contains("health") || message.text.contains("/health") || message.text.contains("endpoint")
```
