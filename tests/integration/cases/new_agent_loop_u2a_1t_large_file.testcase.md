---
target: agent-loop
---

## Storyline
<!-- The agent should create a large file and report its size -->

## User Message
Create a 1MB file at /tmp/test_large.bin filled with zeros.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("1") && (message.text.contains("MB") || message.text.contains("byte") || message.text.contains("1048576") || message.text.contains("1024"))
```
