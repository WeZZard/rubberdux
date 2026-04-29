---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for environment variable values -->
<!-- The agent should use bash to read env vars -->

<!-- The agent should report the value of HOME -->
## User Message
What is the value of the HOME environment variable?

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("HOME") || message.text.contains("/")
```
