---
target: telegram-channel
features:
  - vm
---

## Storyline
<!-- The agent should support reply-to message threading in Telegram -->
<!-- The agent should maintain context across threaded replies -->

## User Message
Tell me a joke.

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should tell a joke -->

## User Message
<!-- Reply to the previous message: "Tell me another one." -->

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should tell another joke while maintaining the joke-telling context -->
