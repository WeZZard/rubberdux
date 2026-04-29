---
target: telegram-channel
---

## Storyline
<!-- The agent should use grep to search file contents -->
<!-- The agent should report matching lines -->

<!-- The agent should report files containing "fn main" -->
## User Message
Search for "fn main" in all Rust files in the src directory.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "grep" || t.name == "bash")
```

## Assistant Message
```cel
message.text.contains("fn main") || message.text.contains(".rs")
```
