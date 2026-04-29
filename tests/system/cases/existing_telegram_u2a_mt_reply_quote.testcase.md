---
features:
  - vm
target: telegram-channel
features:
  - vm
---
features:
  - vm

## Storyline
<!-- The agent should support reply-to quotes in Telegram -->
<!-- The agent should reference the quoted message in the response -->

## User Message
What is the weather today?

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should respond about weather or ask for location -->

## User Message
<!-- Reply to the previous message with a quote: "What about tomorrow?" -->

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should reference the quoted message and answer about tomorrow's weather -->
