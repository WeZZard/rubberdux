---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for network connectivity info -->
<!-- The agent should use bash to check network info -->

## User Message
What is the IP address of this machine?

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should report the IP address to the user -->
