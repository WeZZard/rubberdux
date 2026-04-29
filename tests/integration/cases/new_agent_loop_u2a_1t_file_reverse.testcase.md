---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and reverse its line order -->

## User Message
Create /tmp/test_reverse.txt with "line1\nline2\nline3" and reverse the line order.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash") || message.tool_calls.exists(t, t.name == "write_file")
```

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("line3") && message.text.contains("line1")
```
