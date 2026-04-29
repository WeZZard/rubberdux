---
target: telegram-channel
---

## Storyline
<!-- The agent should execute a bash command and report the output -->
<!-- The agent should handle command output safely -->
<!-- The assistant should use the bash tool -->
<!-- The assistant should call bash with the echo command -->

## User Message
Run "echo hello_from_bash" and show me the output.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
```cel
message.text.contains("hello_from_bash")
```
