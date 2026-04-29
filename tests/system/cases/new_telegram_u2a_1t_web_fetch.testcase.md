---
target: telegram-channel
---

## Storyline
<!-- The agent should fetch a webpage and summarize its content -->
<!-- The agent should report the fetched page content clearly -->

## User Message
Fetch https://example.com and tell me what the page says.

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "web_fetch" || t.name == "bash")
```

## Assistant Message
<!-- The assistant should summarize the page content -->
```cel
message.text.contains("Example") || message.text.contains("example")
```
