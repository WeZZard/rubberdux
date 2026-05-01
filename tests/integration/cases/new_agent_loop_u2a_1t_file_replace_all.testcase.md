---
target: agent-loop
timeout: 120
---

## Storyline
<!-- The agent should create a file and replace all occurrences of a word -->

## User Message
Create /tmp/test_replace.txt with "hello world hello" and replace all "hello" with "hi".

## Tool Call

```cel
message.tool_calls.size() > 0
```

## Assistant Message

```cel
message.text_lower.contains("hi world hi") || message.text_lower.contains("hi")
```
