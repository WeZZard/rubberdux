---
target: telegram-channel
---

## Storyline
<!-- The agent should dispatch a task to a subagent when explicitly requested -->
<!-- The subagent should execute the requested command -->
<!-- The main agent should report the subagent's result to the user -->

## User Message
Use a computer-use agent to run "echo hello_from_child" and report the result.

## Tool Call
```cel
message.tool_calls.exists(t, t.name == "agent")
```

## Assistant Message
```cel
message.text.contains("hello_from_child")
```
