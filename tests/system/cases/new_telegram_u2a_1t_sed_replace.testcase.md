---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a file sed replacement -->
<!-- The agent should use bash sed to replace text -->

<!-- The agent should confirm the final content -->
## User Message
Create /tmp/test_sed.txt with "hello world" and use sed to replace "world" with "rubberdux".

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
```cel
message.text.contains("rubberdux")
```
