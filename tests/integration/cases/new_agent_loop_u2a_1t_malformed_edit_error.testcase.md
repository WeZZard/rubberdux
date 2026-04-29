---
target: agent-loop
---

## Storyline
<!-- The agent should report that a replacement string was not found in the file -->

## User Message
Edit /tmp/test_malformed.txt to replace "foo" with "bar". (Assume the file does not contain "foo".)

## Assistant Message

```cel
message.tool_calls.size() > 0
```

## Assistant Message

```cel
message.text.contains("not") || message.text.contains("error") || message.text.contains("found") || message.text.contains("exist") || message.text.contains("No such") || message.text.contains("fail")
```
