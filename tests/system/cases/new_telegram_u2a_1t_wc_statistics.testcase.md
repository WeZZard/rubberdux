---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file wc statistics -->
<!-- The agent should use bash wc to get statistics -->

<!-- The agent should report line, word, and byte counts -->
## User Message
Get the line, word, and byte count of Cargo.toml.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should report line, word, and byte counts -->
