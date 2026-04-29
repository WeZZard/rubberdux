---
target: agent-loop
---

## Storyline
<!-- The agent should continue from an existing session context -->
<!-- The agent should reference prior conversation turns -->

## User Message
What is my name?

## Assistant Message

```cel
message.tool_calls.size() == 0
```

<!-- The assistant should indicate it does not know the user's name yet -->

## User Message
My name is Alice.

## Assistant Message

```cel
message.text.contains("Alice")
```

## User Message
What is my name?

## Assistant Message

```cel
message.text.contains("Alice")
```
