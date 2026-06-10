---
target: agent-loop
---

## Storyline
<!-- A freshly created whiteboard App runs its originating greeting turn through the agent loop: a plain greeting needs no tools and produces a text reply. -->
<!-- This mirrors the first user turn a new App's worker drives on creation. -->

## User Message
Hi there — this is the first message in a brand new app. Please greet me back.

## Assistant Message

```cel
message.tool_calls.size() == 0
```

<!-- The assistant should respond with a friendly greeting and no tool calls. -->
