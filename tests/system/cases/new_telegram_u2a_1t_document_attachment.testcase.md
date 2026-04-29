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
<!-- The agent should handle document attachments in Telegram -->
<!-- The agent should read and summarize the document -->

## User Message
<!-- Send a text document attachment with a caption "Summarize this document." -->

## Assistant Message
```cel
message.text.size() > 0
```
<!-- The assistant should acknowledge the document attachment -->
<!-- The assistant should read and summarize the document content -->
