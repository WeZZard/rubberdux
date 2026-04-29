---
target: telegram-channel
timeout: 120
---

## Storyline
<!-- The agent should use multiple tools together to solve a complex task -->
<!-- The agent should use glob and grep to search Rust files -->
<!-- The agent should report the final result after all tools complete -->

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
