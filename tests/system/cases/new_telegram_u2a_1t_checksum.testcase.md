---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file checksum -->
<!-- The agent should use bash to compute a checksum -->
<!-- The agent should report the SHA-256 checksum result -->

## User Message
Create /tmp/test_checksum.txt with "checksum test" and compute its SHA-256 checksum.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should compute and report the SHA-256 checksum result -->
```cel
message.text.contains("SHA") || message.text.contains("sha") || message.text.contains("checksum")
```
