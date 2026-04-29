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
<!-- The agent should handle callback queries from inline buttons in Telegram -->
<!-- The agent should process the callback and update the message -->

## User Message
Show me options for pizza toppings.

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should present pizza topping options as inline buttons -->

## User Message
<!-- Click the "Pepperoni" button -->

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should acknowledge the "Pepperoni" selection and update the message or send a confirmation -->
```cel
message.text.contains("epperoni")
```
