---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a large file creation -->
<!-- The agent should use bash to create a large file -->

<!-- The agent should report the file size -->
## User Message
Create a 1MB file at /tmp/test_large.bin filled with zeros.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("1") && (message.text.contains("MB") || message.text.contains("byte") || message.text.contains("1048576") || message.text.contains("1,048,576"))
```
