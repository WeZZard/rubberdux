---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for help with a non-existent file -->
<!-- The agent should report the error clearly -->

## User Message
Read the file at /tmp/nonexistent_file_12345.txt and tell me its contents.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("not") || message.text.contains("exist") || message.text.contains("found") || message.text.contains("error") || message.text.contains("Error")
```
<!-- The assistant should not hallucinate file contents -->
