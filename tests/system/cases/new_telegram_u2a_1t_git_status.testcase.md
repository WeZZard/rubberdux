---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a git status -->
<!-- The agent should use bash to run git status -->

<!-- The agent should report the git status -->
## User Message
What is the git status of this repository?

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should report the git status -->
```cel
message.text.contains("branch") || message.text.contains("clean") || message.text.contains("modified") || message.text.contains("git")
```
