---
name: launch
description: Rebuild rubberdux, start a fresh session as a background process, and show startup log
allowed-tools: Bash, Read
---

# Launch Rubberdux

Rebuild and launch rubberdux with a new session. The previous session is archived.

Run the launch script:

```
${CLAUDE_SKILL_DIR}/scripts/launch.sh
```

After the script completes, report:
- Whether the build succeeded
- The PID of the running process
- The log file path
- Any errors from the startup log
