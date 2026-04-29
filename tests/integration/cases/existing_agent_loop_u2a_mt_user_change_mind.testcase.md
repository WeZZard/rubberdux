---
target: agent-loop
---

## Storyline
<!-- The agent should handle a multi-turn conversation where the user changes their mind -->
<!-- The agent should adapt to the new request without confusion -->

## User Message
Write "hello" to /tmp/test_change.txt.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "write_file") || message.tool_calls.exists(t, t.name == "bash")
```

## User Message
Actually, write "goodbye" to /tmp/test_change.txt instead.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "write_file") || message.tool_calls.exists(t, t.name == "bash")
```

<!-- The assistant should handle the updated request appropriately -->
