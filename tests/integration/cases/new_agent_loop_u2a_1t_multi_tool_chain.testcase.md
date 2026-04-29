---
target: agent-loop
timeout: 120
---

## Storyline
<!-- The agent should use multiple tools together to find Rust files containing "fn main" -->

## User Message
Find all Rust files in the src directory, then search for "fn main" in them and tell me which files contain it.

## Assistant Message

```cel
message.tool_calls.size() > 0
```

## Assistant Message

```cel
message.text.contains("fn main") || message.text.contains(".rs")
```
