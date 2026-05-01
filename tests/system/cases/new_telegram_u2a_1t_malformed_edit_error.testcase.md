---
target: telegram-channel
---

## Storyline
<!-- The agent should create a file and then fail to replace a string that is not present -->
<!-- The agent should report that the replacement string was not found -->

## User Message
First, create /tmp/test_malformed.txt with the content "hello world". Then edit it to replace "foo" with "bar".

## Tool Call
```cel
message.tool_calls.size() > 0
```

## Assistant Message

```cel
message.text_lower.contains("not") || message.text_lower.contains("error") || message.text_lower.contains("found") || message.text_lower.contains("fail")
```

<!-- The assistant should report that the replacement string was not found -->
