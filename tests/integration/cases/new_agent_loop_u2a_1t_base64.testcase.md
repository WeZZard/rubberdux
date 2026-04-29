---
target: agent-loop
---

## Storyline
<!-- The agent should base64 encode and decode a string and present both results -->

## User Message
Base64 encode the string "hello world" and then decode it back.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("aGVsbG8gd29ybGQ") && message.text.contains("hello world")
```
