---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a calculation -->
<!-- The agent should provide the correct answer -->

## User Message
What is 12345 multiplied by 67890?

## Assistant Message
```cel
message.tool_calls.size() == 0
```
<!-- The assistant should provide the correct product -->
```cel
message.text.contains("838102050")
```
