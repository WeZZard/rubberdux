---
target: agent-loop
---

## Storyline
<!-- The agent should create a nested directory structure -->

## User Message
Create the directory /tmp/test_nested/a/b/c.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("test_nested") || message.text.contains("director") || message.text.contains("created") || message.text.contains("mkdir")
```

<!-- The assistant should confirm or report the directory creation -->
