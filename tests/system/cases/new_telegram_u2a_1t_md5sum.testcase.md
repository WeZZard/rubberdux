---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file md5sum -->
<!-- The agent should use bash to compute md5sum -->

<!-- The agent should report the MD5 checksum -->
## User Message
Create /tmp/test_md5.txt with "md5 test" and compute its MD5 checksum.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("MD5") || message.text.contains("md5") || message.text.contains("checksum")
```
