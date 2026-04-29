---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and compute its MD5 checksum -->

## User Message
Create /tmp/test_md5.txt with "md5 test" and compute its MD5 checksum.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "write_file")
```

## Assistant Message

```cel
message.text.matches("[0-9a-fA-F]{32}")
```

<!-- The assistant should report the MD5 checksum -->
