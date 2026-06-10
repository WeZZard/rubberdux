# Tool: Interaction

## Context

The `src/tool/interaction/` module provides four agent-facing tools that let a
running agent raise the unified interaction vocabulary defined in
`src/agent/interaction.rs`. Each tool maps to exactly one variant of
`AgentInteraction`, enqueues the request on the existing `InteractionQueue`,
and awaits the resolved `InteractionResponse`.

## Tools

| Tool name         | Raises                            | Awaits                          |
|-------------------|-----------------------------------|---------------------------------|
| `request_approval`| `AgentInteraction::Approval`      | `InteractionResponse::Approved` or `Declined` |
| `ask_question`    | `AgentInteraction::Question`      | `InteractionResponse::Answered` |
| `offer_choice`    | `AgentInteraction::Choice`        | `InteractionResponse::Answered` |
| `present_preview` | `AgentInteraction::Preview`       | `InteractionResponse::Acknowledged` (or bridged) |

## Design Decisions

### D1 — Reuse the existing `InteractionQueue`

The `InteractionQueue` in `src/agent/external/interaction_queue.rs` already
provides the oneshot-correlation pattern needed by these tools. Rather than
introduce a second queue, the tools bridge `AgentInteraction` → `UIInteractionRequest`
(via the `From` conversions in `src/agent/external/mod.rs`), enqueue via
`InteractionQueue::add`, and bridge the `UIInteractionResponse` back to
`InteractionResponse` (again via existing `From` conversions). This keeps the
queue as the single enqueue point and avoids duplicating the correlation logic.

### D2 — Tools are constructed with `app_id`

Each tool is constructed with an `app_id` string that identifies the
agent/app context. This `app_id` is embedded in every `AgentInteraction` it
raises, so the supervisor can route responses back to the right context.
Tools are not self-contained stateless functions; they are collaborators with
the agent runtime.

### D3 — Trajectory event on raise, not on resolve

Each tool records a `tool.interaction.raised` trajectory event immediately when
it enqueues the interaction, before awaiting. Recording on raise means the
event is observable even if the tool never receives a response (e.g. process
restart). The response is part of the caller's context, not the tool's.

### D4 — Blocking (foreground) execution

All four tools block the calling task until a response arrives. This is
intentional: the agent asks a question and cannot proceed without an answer.
The `ToolOutcome::Immediate` variant is returned once the response is received.
If the queue owner is torn down before responding, the oneshot receiver is
dropped and the tool returns an error outcome.

## Rejected Alternatives

### R1 — Separate unified queue

An alternative was to introduce a new `AgentInteractionQueue<AgentInteraction, InteractionResponse>`
that operates directly on the unified types. This was rejected because:
- The existing `InteractionQueue` already satisfies the need.
- Adding a second queue would require wiring a second queue through the runtime.
- The bridge `From` conversions already exist and are tested.

### R2 — Inline JSON schema strings

Early drafts used `include_str!("*.json")` files like other tools. This was
rejected for the interaction tools because all four schemas are simple and
uniform; keeping them inline in `ToolDefinition::new(...)` calls reduces the
file count without losing readability.
