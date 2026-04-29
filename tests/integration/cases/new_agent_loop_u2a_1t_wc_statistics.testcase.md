---
target: agent-loop
---

## Storyline
<!-- The agent should use wc to get line, word, and byte counts -->

## User Message
Get the line, word, and byte count of Cargo.toml.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.matches("[0-9]+")
```

<!-- The assistant should report line, word, and byte counts -->
