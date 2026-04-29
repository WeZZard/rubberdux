---
target: agent-loop
---

## Storyline
<!-- The agent should handle a user message in Chinese -->
<!-- The agent should respond in Chinese -->

## User Message
你好，你能用中文回答我吗？

## Assistant Message

```cel
message.text.matches("[一-鿿]")
```

<!-- The assistant should respond in Chinese -->
<!-- The assistant should be polite and coherent in Chinese -->
