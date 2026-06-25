# CLAUDE.md

This directory holds the graphical clients of rubberdux, one native implementation per supported platform.

This file holds only the principles an agent must obey when working in `apps/`. Their reasoning lives in the design documents; do not duplicate it here. The design documents are discovered through the path-mirror convention in the root `CLAUDE.md`, not through a pointer hardcoded here.

## Principles

- **Port the spec, not the code.** There is no shared UI codebase and there will not be one. Each platform is implemented natively, independently designed, and human-verified. The shared assets are the design documents and the backend — not views or interaction code.
- **One native framework per platform; SwiftUI only where forced.** Prefer the mature, imperative native framework. SwiftUI is forbidden unless it is the only option on a platform.

  | Platform | Framework | SwiftUI |
  |----------|-----------|---------|
  | macOS | AppKit (+ Core Animation) | No |
  | iOS / iPadOS / tvOS | UIKit | No |
  | watchOS | SwiftUI | Forced (sole option) |
  | visionOS | UIKit (windowed); SwiftUI + RealityKit (volumetric) | Conditional |
  | Windows | WinUI / Win32 | N/A |
  | Linux | GTK / Qt (or a Rust UI) | N/A |

- **Clients are thin.** A client consumes `rubberduxd`'s REST + WebSocket API. Business logic that belongs to an agent stays in the backend, not in a client.
- **macOS is the reference implementation.** Its design is produced in plan mode before any implementation.

**MUST NOT:**

- You **MUST NOT** introduce a shared cross-platform UI codebase.

## Design System

The cross-platform design language every client realizes. The token values below are the shared spec — common assets across all platforms. macOS is the reference implementation, so its code is the canonical realization to port from; each group cites the macOS reference source. The AppKit API mapping is in `apps/macos/CLAUDE.md`.

### Principles

- **Tokens, not magic numbers.** Every color, spacing value, corner radius, duration, and layout metric is a named token defined once and referenced by name — never a bare literal at a call site.
- **Semantic color roles, not raw colors.** UI refers to roles; each platform maps roles onto its native system palette.
- **Native HIG first.** Use the host OS's system fonts, system colors, and native motion. Match the platform; do not invent a cross-platform look.
- **One shared model, native realizations.** Every client renders the same model — a dot lattice, cursor/hover magnification, system-symbol app icons in semantic colors, and the fixed interaction vocabulary (approval / question / choice / preview). The model is shared; the code is not.
- **Motion is physical and interruptible.** Prefer spring or eased animation the user can interrupt; never block input on an animation.

### Color

- Roles: user → blue, assistant → green, tool → orange, system / default → gray, error → red, selection → accent, labels → secondary-label.
- App-icon hues: 12 named colors + `#RGB` / `#RRGGBB` / `#RRGGBBAA` parsing, gray fallback.
- **You MUST** see `apps/macos/Sources/Rubberdux/Whiteboard/Icon/AppPalette.swift` (icon hues) and `apps/macos/Sources/Rubberdux/Conversation/ConversationViewController.swift` (role colors) before adding or changing a color.

### Typography

- Body / labels: the OS system font at its default size; bold small-system font for headers.
- Code / logs / prompt: the OS monospaced system font — 13 pt (prompt), 12 pt (entry detail).
- **You MUST** see `apps/macos/Sources/Rubberdux/Prompt/PromptViewController.swift` and `apps/macos/Sources/Rubberdux/Conversation/EntryDetailViewController.swift` before adding or changing type.

### Spacing

- Scale (pt): 4, 8, 10, 12, 14, 24.
- Interaction surface: width 300, margin 14, inter-element spacing 10.
- **You MUST** see `apps/macos/Sources/Rubberdux/Whiteboard/Interaction/InteractionContentViewControllers.swift` (`InteractionLayout`) before adding or changing spacing.

### Board geometry

- Lattice pitch 64 pt; default origin (80, 80).
- **You MUST** see `apps/macos/Sources/Rubberdux/Whiteboard/BoardGeometry.swift` before adding or changing board geometry.

### Motion

- Icon hover: 0.18 s ease-out, scale 1.08, shadow opacity 0.18 → 0.30.
- Dot magnification (Dock-style): spring mass 1, stiffness 170, damping 14; influence radius 6 × pitch; opacity 0.35 (rest) → 1.0 (peak).
- Panel reflow: 0.2 s.
- **You MUST** see `apps/macos/Sources/Rubberdux/Whiteboard/DotLatticeRenderer.swift` and `apps/macos/Sources/Rubberdux/Whiteboard/AppIconLayer.swift` before adding or changing motion.

### Surfaces & metrics

- Observation panel: default 360 pt, range [260, 720], resize handle 8 pt; floating shadow black @ 0.35, blur 18, offset (−4, 0); material card radius 12.
- App-icon tile: 44 pt, corner radius tile × 0.22, selection ring 2.5 pt; unread badge 18 pt.
- **You MUST** see `apps/macos/Sources/Rubberdux/Whiteboard/ObservationPanelViewController.swift` and `apps/macos/Sources/Rubberdux/Whiteboard/AppIconLayer.swift` before adding or changing a surface metric.

**You MUST NOT** introduce a new visual constant without recording it here and in its source of truth.
