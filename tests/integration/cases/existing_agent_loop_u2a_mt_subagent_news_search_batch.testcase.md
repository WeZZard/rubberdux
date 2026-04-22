---
target: agent-loop
timeout: 60
---

## Storyline
<!-- The agent should be able to describe its running environment when multiple user messages are batched -->
<!-- The agent should be able to spawn a subagent to search for news when user messages arrive in rapid succession -->
<!-- The agent should report the news results back to the user after processing both requests -->

## User Message
Show me the information of your running environment

## User Message
Spawn a subagent to search the latest news of Google

## CHECK: Assistant Message
<!-- The assistant should describe its running environment (OS, architecture, runtime) and reference the user's request for environment information -->

## CHECK: Assistant Message
<!-- The assistant should confirm that a subagent has been dispatched and reference the user's request to search Google news -->

## CHECK: Assistant Message
<!-- The assistant should present the news results obtained from the subagent -->
