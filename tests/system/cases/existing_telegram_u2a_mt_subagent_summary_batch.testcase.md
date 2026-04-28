---
target: telegram-channel
timeout: 240
---

## Storyline
<!-- The agent should be able to address an environment question via Telegram when multiple user messages are batched -->
<!-- The agent should be able to spawn a subagent for a Google summary when user messages arrive in rapid succession -->
<!-- The agent should report the subagent result back to the user via Telegram after processing both requests -->

## User Message
Briefly describe the tool execution environment at a high level without running shell commands.

## User Message
Spawn a subagent to write one concise sentence explaining what Google is. The subagent should not use web search or other tools.

## CHECK: Assistant Message
<!-- The assistant should address the environment request and dispatch a subagent for the Google summary request -->

## CHECK: Assistant Message
<!-- The assistant should present the concise Google summary obtained from the subagent -->
