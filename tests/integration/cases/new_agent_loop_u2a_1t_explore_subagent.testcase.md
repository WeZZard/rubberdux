---
target: agent-loop
timeout: 240
---

## Storyline
<!-- The agent should explore the project structure and summarize it -->

## User Message
Explore the src directory and tell me about the project structure.

## Assistant Message

```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should present a coherent overview of the src directory and relevant project structure findings -->
