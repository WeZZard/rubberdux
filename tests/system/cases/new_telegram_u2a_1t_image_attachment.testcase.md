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
<!-- The agent should handle image attachments in Telegram -->
<!-- The agent should describe or analyze the image -->

## User Message
<!-- Send an image attachment with a caption "What is in this image?" -->

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should acknowledge the image attachment -->
<!-- The assistant should describe or analyze the image content -->
