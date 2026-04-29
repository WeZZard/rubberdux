---
target: agent-loop
---

## Storyline
<!-- The agent should recursively copy a directory -->

## User Message
Copy the src directory to /tmp/test_src_copy.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("cop") || message.text.contains("src") || message.text.contains("test_src_copy") || message.text.contains("success") || message.text.contains("done")
```

<!-- The assistant should confirm or report the copy operation result -->
