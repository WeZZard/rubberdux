---
target: agent-loop
---

## Storyline
<!-- The agent should create a JSON file and read it back -->

## User Message
Create /tmp/test_json.json with '{"name": "test", "value": 42}' and read it back.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "write_file") || message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("test") && message.text.contains("42")
```
