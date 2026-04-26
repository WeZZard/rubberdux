---
target: agent-loop
timeout: 240
---

## Storyline
<!-- The agent should be able to briefly describe the tool execution environment -->
<!-- The agent should be able to spawn a subagent to search for news -->
<!-- The agent should report the news results back to the user -->

## User Message
Briefly describe the tool execution environment at a high level without running shell commands.

## CHECK: Assistant Message
<!-- The assistant should briefly describe the tool execution environment and reference the user's environment question -->

## User Message
Spawn a subagent to search the latest news of Google

## CHECK: Assistant Message
<!-- The assistant should dispatch a subagent for the user's Google news search request -->

## CHECK: Assistant Message
<!-- The assistant should present the news results obtained from the subagent -->
