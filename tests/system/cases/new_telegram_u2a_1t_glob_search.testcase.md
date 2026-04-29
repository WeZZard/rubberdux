---
target: telegram-channel
---

## Storyline
<!-- The agent should use glob to find files matching a pattern -->
<!-- The agent should report the matching files -->

## User Message
Find all Rust source files in the src directory using glob.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "glob" || t.name == "bash")
```

## Assistant Message
```cel
message.text.contains(".rs")
```
