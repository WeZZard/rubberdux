---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file sha1sum -->
<!-- The agent should use bash to compute sha1sum -->

<!-- The agent should report the SHA-1 checksum -->
## User Message
Create /tmp/test_sha1.txt with "sha1 test" and compute its SHA-1 checksum.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("SHA") || message.text.contains("sha") || message.text.contains("checksum")
```
