---
target: agent-loop
---

## Storyline
<!-- The agent should use grep to find files containing a specific word -->

## User Message
Find all files in the src directory containing the word "error".

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "grep") || message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should report the files containing "error" -->
