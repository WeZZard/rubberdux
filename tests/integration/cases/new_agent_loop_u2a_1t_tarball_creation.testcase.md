---
target: agent-loop
---

## Storyline
<!-- The agent should create a directory with a file and archive it as a tarball -->

## User Message
Create a tarball /tmp/test_archive.tar.gz containing /tmp/test_archive_dir with a file inside.

## Assistant Message

```cel
message.tool_calls.exists(t, t.name == "bash")
```

## Assistant Message

```cel
message.text.contains("tar") || message.text.contains("archive") || message.text.contains(".gz")
```
