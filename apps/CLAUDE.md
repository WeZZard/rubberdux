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
