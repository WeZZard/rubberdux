# macOS Whiteboard Client — Agent-Initiated Interaction Surfaces

This document is the design record for the agent-initiated interaction surfaces
of the macOS whiteboard client. It governs:

- `apps/macos/Sources/Rubberdux/Whiteboard/Interaction/InteractionContentViewControllers.swift`
- `apps/macos/Sources/Rubberdux/Whiteboard/Interaction/InteractionPopoverController.swift`
- `apps/macos/Sources/Rubberdux/Whiteboard/Interaction/PendingInteractionsController.swift`
- `apps/macos/Sources/Rubberdux/Whiteboard/Interaction/PendingInteractionsStore.swift`
- `apps/macos/Sources/Rubberdux/Networking/InteractionSocket.swift`
- the additive interaction wiring in
  `apps/macos/Sources/Rubberdux/Whiteboard/WhiteboardViewController.swift`

Read [`docs/apps/macos/whiteboard-client.md`](whiteboard-client.md) for the
models and networking these surfaces reuse, and
[`docs/gateway/apps_stream.md`](../../gateway/apps_stream.md) for the backend
interaction WebSocket shape this client mirrors.

## Context

An App's worker raises an interaction when it needs a human decision. The fixed
vocabulary is four primitives — Approval, Question, Choice, Preview — modeled in
`Model/AgentInteraction.swift` and answered with `Model/InteractionResponse.swift`.
These surfaces present a raised interaction and return the human's answer.

## Where the interaction appears (D1: front vs. not-front)

The placement of a raised interaction depends on whether the board window is the
front, key surface when the interaction arrives:

- **Board is front** → an `NSPopover` (`InteractionPopoverController`) anchored
  to the icon's rect on the board. The human answers inline next to the icon
  that raised the interaction. The anchor rect comes from
  `BoardView.iconRect(forAppID:)`, which is edge-clamped to the board's visible
  bounds so the popover never points off-screen.
- **Board is not front** → a count badge on the icon's `AppIconLayer` (the badge
  sublayer already exists). Clicking a badged icon opens the pop-out pending list
  (`PendingInteractionsController`).

Front-surface state is tracked in `WhiteboardViewController` via window
key/main and `NSApplication` activation notifications. This is the sole input to
the popover-vs-badge decision.

## One vocabulary, customization in data (D2)

There is one view-controller class per primitive
(`InteractionContentViewControllers.swift`); each renders purely from the
request's data. An App never ships bespoke interaction view code — every
customization a worker needs is expressed in the interaction's data (prompt,
options, artifact). `InteractionContentViewController.make(for:onRespond:)` is
the single mapping from an `AgentInteraction` to its content view controller, and
is reused by both the popover and the pending list so the two surfaces show
identical content.

## Pending model

`PendingInteractionsStore` is the pure, AppKit-free source of truth for which
interactions each App is awaiting. It derives the icon badge count and the
pending-list contents from the same data, so the two never diverge. It is
keyed by app id then request id: a re-raise of the same request is idempotent,
and a resolve removes exactly one. Ordering within an App is first-seen so the
list does not reshuffle as items resolve. This separation keeps the badge-count
and list-derivation logic unit-testable without UI.

## Transport

`InteractionSocket` is a bidirectional per-App WebSocket subscription to
`/api/v1/ws/apps/{id}/interactions`, mirroring `InteractionWsMessage` /
`InteractionInbound` in `src/gateway/apps_stream.rs`. Outbound it decodes
`interaction_raised` / `resolved`; inbound it sends a `respond` frame. The
backend replays an App's already-pending interactions on connect, so a client
that subscribes after a raise still sees it.

`WhiteboardViewController` opens one `InteractionSocket` per App on the board and
closes it when the App leaves, so raised/resolved events reach the board without
requiring the App to be selected. Responses are sent over that socket while it
is open; `APIClient.respondToInteraction` is the REST fallback. The board-level
`badge` frame from `/ws/board` is ignored on this client because the per-App
socket, via `PendingInteractionsStore`, is the authoritative count source.

## Rejected alternatives

- **Drive badges from the board-level `badge` frame.** Rejected: that frame
  collapses the count to 1/0 and would clobber the precise per-App count the
  interaction socket already carries. The per-App socket is authoritative.
- **A separate window or sheet for interactions.** Rejected: it detaches the
  interaction from the icon that raised it and competes with the board for focus.
  The popover (front) and badge (not-front) keep the interaction tied to its App.
