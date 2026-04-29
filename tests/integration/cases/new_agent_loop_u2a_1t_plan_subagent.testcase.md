---
target: agent-loop
timeout: 240
---

## Storyline
<!-- The agent should dispatch a plan subagent and report its proposed steps -->

## User Message
Use a plan subagent to propose three concise steps for adding a tiny `/health` REST endpoint in Rust, then report the three steps.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "dispatch_subagent") || message.tool_calls.exists(t, t.name == "agent")
```

## Assistant Message

```cel
message.text.contains("1") && message.text.contains("2") && message.text.contains("3")
```

<!-- The assistant should present the concise three-step plan produced by the subagent to the user -->
