---
target: agent-loop
---

## Storyline
<!-- The agent should create a target file and a symlink pointing to it -->

## User Message
Create a symlink /tmp/test_link.txt pointing to /tmp/test_target.txt with content "target".

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("symlink") || message.text.contains("link") || message.text.contains("created") || message.text.contains("point")
```

<!-- The assistant should confirm or report the symlink creation -->
