---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a base64 encode/decode -->
<!-- The agent should use an appropriate tool for base64 operations -->
<!-- The assistant should complete both the base64 encoding and decoding -->

## User Message
Base64 encode the string "hello world" and then decode it back.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("aGVsbG8gd29ybGQ=") || message.text.contains("aGVsbG8gd29ybGQ")
```
```cel
message.text.contains("hello world")
```
