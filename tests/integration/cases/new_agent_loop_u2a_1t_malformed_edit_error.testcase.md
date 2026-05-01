---
target: agent-loop
---

## Storyline
<!-- The agent should create a file and then fail to replace a string that is not present -->

## User Message
First, create /tmp/test_malformed.txt with the content "hello world". Then edit it to replace "foo" with "bar".

## Tool Call

```cel
message.tool_calls.size() > 0
```

## Assistant Message

```cel
message.text.contains("not") || message.text.contains("error") || message.text.contains("found") || message.text.contains("exist") || message.text.contains("No such") || message.text.contains("fail")
```
