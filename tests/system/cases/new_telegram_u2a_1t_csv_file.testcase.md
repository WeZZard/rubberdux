---
target: telegram-channel
---

## Storyline
<!-- The agent should handle a user asking for a CSV file read -->
<!-- The agent should read and report the CSV file contents -->

<!-- The agent should report the CSV contents -->
## User Message
Create /tmp/test_csv.csv with "name,age\nAlice,30\nBob,25" and read it back.

## Assistant Message
```cel
message.tool_calls.size() > 0
```

## Assistant Message
```cel
message.text.contains("Alice") && message.text.contains("Bob")
```
