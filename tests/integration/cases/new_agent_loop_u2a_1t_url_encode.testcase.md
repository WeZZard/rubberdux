---
target: agent-loop
---

## Storyline
<!-- The agent should URL encode and decode a string -->

## User Message
URL encode the string "hello world" and then decode it back.

## Assistant Message

```cel
message.text_lower.contains("hello%20world") || message.text_lower.contains("hello+world")
```

<!-- The assistant should also confirm the decoded result matches the original string -->
