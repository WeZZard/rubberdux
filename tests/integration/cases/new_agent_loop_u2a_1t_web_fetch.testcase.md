---
target: agent-loop
---

## Storyline
<!-- The agent should fetch a webpage and summarize its content -->

## User Message
Fetch https://example.com and tell me what the page says.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "web_fetch") || message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message
<!-- The assistant should summarize the page content -->
