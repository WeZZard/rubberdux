---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and compress it with gzip -->

## User Message
Create /tmp/test_compress.txt with "compress me" and compress it with gzip.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "write_file")
```

## Assistant Message

```cel
message.text.contains("gz") || message.text.contains("compress")
```
