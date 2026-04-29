---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and compute its SHA-256 checksum -->

## User Message
Create /tmp/test_checksum.txt with "checksum test" and compute its SHA-256 checksum.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "write_file")
```

## Assistant Message

<!-- The assistant should report a valid SHA-256 hex checksum string -->
