# Whiteboard GUI — Cross-Platform Design

This is the cross-platform, tech-agnostic design for rubberdux's new graphical client: a **whiteboard** of agent-backed **apps**. It defines the concept, architecture, interaction model, and design language that every platform implementation realizes natively. It is the specification referenced by `apps/CLAUDE.md` and by the per-platform implementations under `apps/<platform>/`.

Per-platform realizations are independently designed, evaluated, and human-verified; this document is what they share, not code they share.

## Context and Goal

The existing macOS GUI is a debugging and monitoring tool: it shows a flat list of entries from a single running agent loop, and there is no first-class concept of a "conversation" or "app" in the backend — entries live in memory for one session (`src/agent/`, `src/gateway/state.rs`).

The goal is a user-facing client where work is durable and addressable: each conversation becomes a persistent **app** with its own identity (icon + title) and a spatial home on a board. The user mostly *observes and approves* autonomous agents rather than chatting turn-by-turn. This is the macOS Launchpad / springboard mental model applied to agents.

## Conceptual Model

- **Whiteboard** — the primary surface: a field of evenly spaced gray dots resembling **dot grid paper** (the bullet-journal stationery where a lattice of dots implies the grid without drawing ruling lines). The grid is implied by the dots, never drawn.
- **App** — a persistent unit on the whiteboard, shown as an icon. An app *is an agent*: it accomplishes tasks while the user observes and approves. Apps are durable; spatial position carries memory.
- **Agent worker** — the subprocess hosting one app's agent, crash-isolated from the host.
- **Host supervisor** — `rubberduxd`, which spawns, supervises, tombstones, and restores agent workers and exposes everything over its API.
- **Tombstoning** — the suspend/restore mechanism bounding memory across thousands of apps: an idle worker's state is persisted and its subprocess killed, then restored on demand, appearing as though it never left. (The term originates with the Windows Phone lifecycle; iOS implements the same idea as Jetsam plus state restoration.)
- **Peer network** — agents form a *decentralized* network; each knows which peers exist, which it may talk to, and when. No central orchestrator.
- **Interaction vocabulary** — the single fixed set of interaction primitives an agent may invoke (Approval, Question, Choice, Preview).

## Architecture

- **Apps are agents.** Creating an app creates a persistent agent (history + config on disk). Opening an app returns to that same agent.
- **One agent-worker subprocess per active app**, crash-isolated from the host: a worker crash never takes down the host.
- **Idle workers are tombstoned.** The board may hold thousands of apps while only a few workers run at once.
- **Agents communicate peer-to-peer.** No central node mediates the network; a decentralized design has a higher ceiling than a star topology around one orchestrator.
- **Clients are thin.** Every platform client consumes `rubberduxd`'s existing API boundary (REST + WebSocket). The client tech stack is therefore decoupled from the Rust backend.

## Interaction Model

Observation and interaction are **separate surfaces**.

**Observation** (passive). The user selects one or more app icons; their conversations and trajectories list in a panel on the right-hand side of the whiteboard, which shrinks the usable board area while open. It is debug-flavored — not a polished conversation UI — and may include a message box for debugging rather than routine chat.

**Interaction** (active, agent-initiated). When an agent needs the user, it raises a request in the fixed **interaction vocabulary**:

- **Approval** — request permission to proceed.
- **Question** — ask the user something.
- **Choice** — offer a list to pick from.
- **Preview / quick look** — present an outcome for inspection.

"Customized UI per app" means customization in the *data* of these primitives (title, options, preview content), not bespoke per-app UI code. Where a request surfaces depends on board state:

- **Board open** → a **pop-over** near the app's icon.
- **Board closed** → a **badge** on the app icon; clicking it pops out a **list of pending interactions** (approvals, questions, result previews, presentations) to process.

## Visual and Interaction Design Language

- **Dot grid field.** Evenly spaced gray dots; grid and ruling are implied, never drawn.
- **Hover magnification.** Hovering magnifies nearby dots in the manner of the macOS Dock. Over an empty cell, a **plus glyph** appears at the center of the magnified region to invite creation.
- **App icon.** A **system-native symbol glyph plus a generated color** derived from the task — instant, free, and coherent across the whole board. AI-generated illustrated icons are out of scope.

## Creation Flow

1. The user hovers an empty cell; dots magnify and a plus glyph appears.
2. The user clicks and provides a task.
3. The backend creates a persistent agent, then derives a **short descriptive title** and an **icon** (symbol glyph + color) from the task.
4. The app takes its place on the board and begins working; the user observes and approves.

## Per-Platform Realization

Prefer the mature, native, imperative UI framework on each platform. **SwiftUI is forbidden unless it is the only option on a platform**, because of its documented behavioral drift across OS major, minor, and patch releases.

| Platform | Framework | SwiftUI |
|----------|-----------|---------|
| macOS | AppKit (+ Core Animation) | No |
| iOS / iPadOS / tvOS | UIKit | No |
| watchOS | SwiftUI | Forced — WatchKit UI is deprecated; a new watch app is SwiftUI-only in practice |
| visionOS | UIKit for a windowed board; SwiftUI + RealityKit only for volumetric content | Conditional |
| Windows | WinUI / Win32 (or a non-SwiftUI native stack) | N/A |
| Linux | GTK / Qt (or a Rust UI such as iced/egui) | N/A |

The dot field is a custom GPU-drawn widget in every option, so the framework choice governs the surrounding chrome (panels, icons, pop-overs, badges) more than the dot field itself.

**macOS is the reference implementation**, built in AppKit with Core Animation for the dot field and Dock-style magnification. Its design is produced in plan mode before implementation, and it serves as the literate specification other platforms are ported from.

## Decisions

Recorded in the spirit of Architecture Decision Records ([arc42 §9](https://docs.arc42.org/section-9/)): context, decision, the alternative that lost, and consequences.

### D1 — Apps are observed-and-approved agents, not chats
**Context.** "Click an icon to enter a conversation" suggested a chat app. **Decision.** An app is a persistent agent that works autonomously while the user observes and approves; there is no full conversation UI, only an observation panel and agent-initiated interactions. **Rejected.** A turn-by-turn chat client — it mismatches how rubberdux's agent loops actually work. **Consequence.** The client is an observer/approver, not a messenger.

### D2 — One fixed interaction vocabulary
**Context.** "Customized UI per app" could mean bespoke per-app code, a declarative UI DSL, or a fixed primitive set. **Decision.** A single fixed vocabulary (Approval, Question, Choice, Preview); customization lives in the data. **Rejected.** Per-app bespoke native UI (does not scale, fights convention-over-configuration) and a declarative UI spec/DSL (needs an interpreter; premature). **Consequence.** Any agent works with zero new UI code per app.

### D3 — Icons are a system glyph plus a generated color
**Context.** The original idea called for a "beautiful generated icon." **Decision.** The agent picks a system-native symbol glyph (SF Symbol on Apple platforms) and a generated color from the task. **Rejected.** AI image-model icons — slow, costly, stylistically inconsistent across a board. **Consequence.** Icons are instant, free, and visually coherent; less unique per app.

### D4 — Agent-worker subprocess with tombstoning
**Context.** The board may hold thousands of apps; the backend today runs one in-process agent loop. **Decision.** One crash-isolated subprocess per active app, tombstoned when idle. **Rejected.** A single shared loop that context-switches (no isolation) and always-on long-lived loops for every app (unbounded resource cost). **Consequence.** A worker crash never takes down the host; memory is bounded by the count of *active*, not *total*, apps.

### D5 — Decentralized peer-to-peer agent network
**Context.** Agents must coordinate and react to each other and the environment. **Decision.** A decentralized network where each agent knows and messages its peers directly. **Rejected.** A central orchestrator agent — a star topology whose ceiling is lower and whose failure is total. **Consequence.** Coordination logic is a per-agent capability, not a privileged node.

### D6 — Port the specification, not the code
**Context.** Targets include macOS, iOS, iPadOS, watchOS, visionOS, Windows, and Linux. **Decision.** No shared UI codebase; each platform is implemented natively against this shared specification and independently human-verified. **Rejected.** A single cross-platform UI framework (Tauri/Flutter/iced/GPUI) — a watchOS board and a visionOS board are genuinely different designs that one codebase would compromise. **Consequence.** This document, the conceptual model, and the backend are the shared assets; views and interaction code are not.

### D7 — Native imperative frameworks; SwiftUI only where forced
**Context.** SwiftUI exhibits behavioral drift across OS releases, and stability across OS versions is required. **Decision.** Use the mature imperative framework per platform; allow SwiftUI only where it is the sole option. **Evidence.** WatchKit storyboards were deprecated in watchOS 7+, with new apps expected in SwiftUI ([Apple TN3157](https://developer.apple.com/documentation/technotes/tn3157-updating-your-watchos-project-for-swiftui-and-widgetkit)); visionOS supports UIKit for windowed content while RealityKit's volumetric `RealityView` requires SwiftUI ([visionOS Overview](https://developer.apple.com/visionos/); [WWDC23, "Meet UIKit for spatial computing"](https://developer.apple.com/videos/play/wwdc2023/111215/)). **Consequence.** macOS uses AppKit; watchOS uses SwiftUI; visionOS depends on whether the board is windowed or volumetric.

### D8 — macOS/AppKit is the reference implementation
**Context.** One platform must be built first to validate the specification. **Decision.** macOS with AppKit + Core Animation, extending the existing app. **Rejected.** iPadOS-first (backend runs on a host; needs remote access) and visionOS-first (forces SwiftUI + RealityKit; hardest to verify). **Consequence.** Lowest-risk path; Core Animation supplies Dock-style magnification natively.
