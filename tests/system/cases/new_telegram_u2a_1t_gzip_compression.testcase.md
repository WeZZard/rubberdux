---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file compression -->
<!-- The agent should use bash to compress the file -->

<!-- The agent should confirm or report the compression result -->
## User Message
Create /tmp/test_compress.txt with "compress me" and compress it with gzip.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("gz") || message.text.contains("gzip") || message.text.contains("compress")
```
