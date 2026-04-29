---
target: telegram-channel
features:
  - vm
---

## Storyline
<!-- The agent should handle deleted messages in Telegram -->
<!-- The agent should not crash and should acknowledge the deletion -->

## User Message
Hello, this message will be deleted.

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should respond normally -->

## User Message
<!-- Delete the previous message -->

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should acknowledge the deletion or ask for clarification without crashing -->
