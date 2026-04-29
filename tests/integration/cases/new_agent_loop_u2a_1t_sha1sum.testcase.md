---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and compute its SHA-1 checksum -->

## User Message
Create /tmp/test_sha1.txt with "sha1 test" and compute its SHA-1 checksum.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "write_file")
```

## Assistant Message

```cel
message.text.matches("[0-9a-fA-F]{40}")
```

<!-- The assistant should report the SHA-1 checksum -->
