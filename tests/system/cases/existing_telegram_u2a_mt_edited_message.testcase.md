---
target: telegram-channel
features:
  - vm
---

## Storyline
<!-- The agent should handle edited messages in Telegram -->
<!-- The agent should respond to the edited content -->

## User Message
What is the capital of France?

## Assistant Message
```cel
message.text.contains("Paris")
```

## User Message
<!-- Edit the previous message to: "What is the capital of Germany?" -->

## Assistant Message
```cel
message.text.contains("Berlin")
```
<!-- The assistant should reference the edited message -->
