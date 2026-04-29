---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a JSON file read -->
<!-- The agent should parse and report JSON contents -->

<!-- The agent should report the JSON contents -->
## User Message
Create /tmp/test_json.json with '{"name": "test", "value": 42}' and read it back.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("test") && message.text.contains("42")
```
