---
target: agent-loop
---

## Storyline
<!-- The agent should execute a bash command and report the output -->
<!-- The agent should handle command output safely -->
<!-- The assistant should use the bash tool -->

## User Message
Run "echo hello_from_bash" and show me the output.

## Tool Call
```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should report the command output including hello_from_bash -->
