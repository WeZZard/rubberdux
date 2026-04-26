---
target: agent-loop
timeout: 240
---

## Storyline
<!-- The agent should be able to address an environment question when multiple user messages are batched -->
<!-- The agent should be able to spawn a subagent to search for news when user messages arrive in rapid succession -->
<!-- The agent should report the news results back to the user after processing both requests -->

## User Message
Briefly describe the tool execution environment at a high level without running shell commands.

## User Message
Spawn a subagent to search the latest news of Google

## CHECK: Assistant Message
<!-- The assistant should address the environment request and dispatch a subagent for the Google news search -->

## CHECK: Assistant Message
<!-- The assistant should present the news results obtained from the subagent -->
