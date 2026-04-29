---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking to edit a file when the replacement string is absent -->
<!-- The agent should report that the requested replacement cannot be applied -->

## User Message
Edit /tmp/test_malformed.txt to replace "foo" with "bar". (Assume the file does not contain "foo".)

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
<!-- The assistant should report that the replacement string was not found -->
