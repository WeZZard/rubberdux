---
target: agent-loop
---

## Storyline
<!-- The agent should create a CSV file and read it back -->

## User Message
Create /tmp/test_csv.csv with "name,age\nAlice,30\nBob,25" and read it back.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "write_file") || message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("Alice") && message.text.contains("Bob")
```
