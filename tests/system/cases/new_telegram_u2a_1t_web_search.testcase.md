---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a web search -->
<!-- The agent should use web_search tool -->

## User Message
Search the web for "Rust programming language".

## Assistant Message
```cel
message.tool_calls.exists(t, t.name == "web_search" || t.name == "$web_search")
```

## Assistant Message
<!-- The assistant should summarize the search results -->
```cel
message.text.contains("Rust") || message.text.contains("rust")
```
