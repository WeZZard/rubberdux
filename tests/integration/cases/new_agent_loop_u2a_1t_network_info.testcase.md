---
target: agent-loop
---

## Storyline
<!-- The agent should retrieve and report the machine's IP address -->

## User Message
What is the IP address of this machine?

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.matches("[0-9]+\\.[0-9]+\\.[0-9]+\\.[0-9]+") || message.text.matches("[0-9a-fA-F]*:[0-9a-fA-F]*:")
```

<!-- The assistant should report the IP address to the user -->
