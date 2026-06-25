# CLAUDE.md

This directory holds the macOS native client of rubberdux, built with AppKit and
Core Animation. It is the reference implementation of the whiteboard model and the
canonical realization of the common Design System.

This file holds only the principles an agent must obey when working in
`apps/macos/`; it carries no design rationale. The design language and its token
values are common assets — see `apps/CLAUDE.md` → Design System.

## Principles

- **AppKit + Core Animation only.** No SwiftUI (see `apps/CLAUDE.md`).
- **Thin client.** Speak `rubberduxd`'s REST + WebSocket API; hold no agent
  business logic.

## Design System (macOS realization)

The token values are common and live in `apps/CLAUDE.md` → Design System, cited to
this client's source files. Only the AppKit API mapping is macOS-specific:

- **Color** — roles map to `NSColor` system colors; app-icon hues via `AppPalette`.
- **Typography** — `NSFont.systemFont` / `NSFont.boldSystemFont` /
  `NSFont.monospacedSystemFont`.
- **Motion** — Core Animation: `CASpringAnimation` (dot magnification),
  `CABasicAnimation` and implicit layer animation (hover, panel reflow).

**You MUST NOT** add a visual constant here without recording it in
`apps/CLAUDE.md` → Design System.
