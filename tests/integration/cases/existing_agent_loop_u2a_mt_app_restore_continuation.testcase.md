---
target: agent-loop
---

## Storyline
<!-- A whiteboard App's worker is tombstoned when idle and restored from its on-disk session history; after a restore the conversation must continue from where it left off. -->
<!-- This case exercises that continuity at the agent loop level: a fact stated before the tombstone must still be known in the post-restore turn. -->

## User Message
For this app, remember that the project codename is Polaris.

## Assistant Message

```cel
message.text.contains("Polaris")
```

## User Message
The app went idle and was just restored. What is the project codename?

## Assistant Message

```cel
message.text.contains("Polaris")
```
