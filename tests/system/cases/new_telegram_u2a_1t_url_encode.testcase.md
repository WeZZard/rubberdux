---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a URL encode/decode -->
<!-- The agent should URL encode the string -->

<!-- The agent should decode and confirm the original string -->
## User Message
URL encode the string "hello world" and then decode it back.

## Assistant Message
```cel
message.text.contains("hello") && message.text.contains("world")
```
