# macOS Whiteboard Client — Models and Networking

This document describes the Swift models, networking layer, and small UI helpers
that back the whiteboard GUI on macOS. It is the design record for:

- `apps/macos/Sources/Rubberdux/Model/App.swift`
- `apps/macos/Sources/Rubberdux/Model/AgentInteraction.swift`
- `apps/macos/Sources/Rubberdux/Model/InteractionResponse.swift`
- `apps/macos/Sources/Rubberdux/Model/BoardEvent.swift`
- `apps/macos/Sources/Rubberdux/Networking/APIClient+Apps.swift`
- `apps/macos/Sources/Rubberdux/Networking/BoardSocket.swift`
- `apps/macos/Sources/Rubberdux/Networking/AppSocket.swift`
- `apps/macos/Sources/Rubberdux/Networking/AppSocketRegistry.swift`
- `apps/macos/Sources/Rubberdux/Whiteboard/Icon/AppPalette.swift`
- `apps/macos/Sources/Rubberdux/Whiteboard/Icon/SymbolImage.swift`

Read [`docs/apps/whiteboard.md`](../whiteboard.md) and
[`docs/gateway/apps.md`](../../gateway/apps.md) for the broader context.

## Models

### App, Icon, BoardPosition

`App` is a Codable mirror of `AppDto` from `src/gateway/apps.rs`. `Icon` mirrors
`IconDto`; `BoardPosition` mirrors `BoardPositionDto`. `AppStatus` mirrors the
`status` string field (`"active"` / `"tombstoned"`).

All snake_case field mappings are explicit in `CodingKeys`.

### AgentInteraction

`AgentInteraction` mirrors `src/agent/interaction.rs::AgentInteraction`. The
backend uses `#[serde(tag = "kind", rename_all = "snake_case")]`, so the wire
discriminator is the string field `"kind"` with values `"approval"`,
`"question"`, `"choice"`, `"preview"`. Each variant carries `request_id` and
`app_id` plus variant-specific fields. `ApprovalFlavor`, `ChoiceOption`, and
`PreviewArtifact` are mirrored directly.

`InteractionResponse` mirrors `src/agent/interaction.rs::InteractionResponse`
with the same `"kind"` tag and variants `"approved"`, `"declined"`,
`"answered"`, `"acknowledged"`. It is `Encodable` so `APIClient+Apps` can POST
it to the backend.

### BoardEvent

`BoardEvent` mirrors `src/app/supervisor.rs::BoardEvent`. The wire
representation uses a `"kind"` field with values `"created"`, `"status_changed"`,
`"moved"`, `"archived"`. The backend WebSocket surface for board events is
designed in the next task; this model is ready for it.

`Entry` and `TrajectoryEvent` from the existing model layer are reused unchanged
as the element types for per-app entry and trajectory snapshots.

## Networking

### APIClient+Apps

The existing `APIClient` is GET-only (it calls `session.data(from:)` which
accepts a `URL`). `APIClient+Apps` adds:

- `post<Body, Response>` — POST with a JSON body, decode response.
- `postVoid<Body>` — POST with a JSON body, discard response (for `202`/`204`).
- `patch<Body, Response>` — PATCH with a JSON body, decode response.
- `deleteVoid` — DELETE, discard response (for `204`).

These helpers are `internal` extension methods so they are accessible in tests
without leaking into the public surface. The existing GET methods and their
private response types are unchanged.

App-specific methods mirror every route in `src/gateway/apps.rs`:
`apps()`, `createApp(task:position:)`, `patchApp(id:position:title:userLocked:)`,
`moveApp(id:position:)`, `archiveApp(id:)`, `entries(appID:)`,
`trajectory(appID:)`, `interactions(appID:)`,
`respondToInteraction(appID:requestID:response:)`.

To make `session` and `decoder` accessible to the extension file, their access
level was widened from `private` to `internal` (Swift default) in
`APIClient.swift`. No external behavior changes.

### BoardSocket

`BoardSocket` connects to `/api/v1/ws/board` (scheme `ws`) and forwards each
decoded `BoardEvent` on a Combine `PassthroughSubject`. The receive loop
reschedules itself after each message, matching the pattern in the existing
`WebSocketClient`. A failure silently stops the loop (same behavior as the
existing client).

### AppSocket

`AppSocket` connects to two per-app WebSocket paths:
`/api/v1/ws/apps/{id}/entries` and `/api/v1/ws/apps/{id}/trajectory`. It emits
`EntryNotification` and `TrajectoryEvent` on separate `PassthroughSubject`
properties, matching the subject-per-stream style of `WebSocketClient`.

### AppSocketRegistry

`AppSocketRegistry` ref-counts open `AppSocket` instances keyed by `appID`.
`open(appID:)` increments the count (creating and connecting the socket on first
open); `close(appID:)` decrements it and disconnects at zero. This ensures
sockets are not leaked when multiple view-controllers subscribe to the same app,
and that the socket is not disconnected while a second subscriber is still active.

The registry is not thread-safe; all calls must occur on the main thread (the
AppKit event loop).

## Icon Helpers

### AppPalette

`AppPalette.color(named:)` maps the `Icon.color` string to `NSColor`. The
backend stores hex strings (e.g. `"#8E8E93"`) from its identity derivation; the
helper also recognises a vocabulary of named colors (`"indigo"`, `"orange"`,
etc.) for forward compatibility. Unknown names fall back to `NSColor.systemGray`.

Hex parsing supports `#RGB`, `#RRGGBB`, and `#RRGGBBAA` forms via a private
`NSColor` initializer extension.

### SymbolImage

`SymbolImage.image(named:)` wraps `NSImage(systemSymbolName:accessibilityDescription:)`.
When the system does not recognise the name, it returns the fallback glyph
`"questionmark.circle"` so callers always receive a valid, non-nil image.

## Decisions

### D1 — Reuse Entry and TrajectoryEvent unchanged

The per-app entry and trajectory snapshots use the same `Entry` and
`TrajectoryEvent` types already defined in the model layer. Duplicating them
would diverge their Codable representations from the single-agent endpoints.
The `entries(appID:)` and `trajectory(appID:)` response wrappers simply
differ in the outer envelope key (`"entries"` vs `"events"`).

### D2 — Kind-tagged enums via manual Codable

The backend's `#[serde(tag = "kind")]` produces a flat JSON object where the
discriminator is an inline `"kind"` field. Swift's synthesized `Codable` does not
support this encoding. Manual `init(from:)` and `encode(to:)` matching the
existing `EntryOrigin` and `Message` style in the model layer are the
appropriate mechanism.

### D3 — Internal session and decoder on APIClient

Widening `session` and `decoder` from `private` to `internal` lets the extension
file in `APIClient+Apps.swift` access them without a redesign. The existing
external callers of `APIClient` see no change; both properties remain
non-public.

### D4 — AppSocketRegistry ref-count, not subscription count

The registry tracks open/close pairs from view-controllers, not Combine
subscription counts. Subscription counts are invisible to the registry and vary
asynchronously; explicit open/close pairs are deterministic and match the AppKit
view lifecycle (viewWillAppear / viewWillDisappear).
