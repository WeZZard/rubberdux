import AppKit
import Combine

// MARK: - SurfaceObservationReporter

/// Makes the macOS whiteboard's UI state a RECORDED input (M-observe). It watches
/// the same surface root the `SurfaceDriveController` drives and feeds the host
/// two stratum-1 inputs over `SurfaceSocket`:
///
/// 1. **Human edits → `SurfaceMutated{op, cause: .human}`.** When the USER
///    directly edits a surface element (a field-editor value edit), it builds the
///    corresponding `SurfaceOp` and sends it stamped `Cause.human`. This is the
///    SOLE `cause = Human` origin (Inv 18): the reporter listens only to genuine
///    user field-editor signals (`NSControl.textDidChangeNotification`), which the
///    OS posts for direct user typing but NOT for the programmatic
///    `setAccessibilityValue`/AX-press that `AccessibilitySurfaceDriveTarget`
///    performs for an agent write. An agent write is therefore never re-reported
///    here as Human — it stays the single `cause = Command` `ToolReturned` M-apply
///    already records, and the worker bridge drops its `Command`-caused echo.
///
/// 2. **Observed AX state → `SurfaceObserved`.** When focus / selection / viewport
///    / window / cursor (or the perceived surface version / `ax_digest`) change, it
///    harvests them from the root's Accessibility subtree and sends a
///    `SurfaceObserved` snapshot, so the World's surface view, the UI-tool
///    `Fingerprint`, and the per-edge mode projection reflect the real UI (Inv 18,
///    19). The `ax_digest` is a deterministic content hash of the observed subtree;
///    the perceived `version` bumps on every perceived change.
///
/// All work runs on the main thread (every read touches AppKit/AX state) and never
/// blocks: the socket send is asynchronous and snapshot emits are coalesced to one
/// per run-loop turn. See docs/agent/world/ecs-runtime.md (Themes 1e/2b, Inv 18/19).
final class SurfaceObservationReporter {

    // MARK: - Collaborators

    /// The outbound surface client. Human mutations and observed snapshots are
    /// sent through its `send(mutation:cause:)` / `send(observation:)` hooks.
    private let socket: SurfaceSocket

    /// The surface root whose AX subtree is observed — the same root
    /// `AccessibilitySurfaceDriveTarget` addresses, so an `ElementId` indexes the
    /// identical flattened subtree on both the apply and observe sides. Weak so the
    /// reporter never keeps the view hierarchy alive past its controller.
    private weak var root: NSView?

    /// The surface this reporter speaks for. The whiteboard hosts a single surface
    /// (its container view), so a fixed id identifies it, matching the
    /// "unobserved = version 0" default in `SurfaceState::unobserved`.
    private let surface: SurfaceId

    /// A guard on the AX walk so a pathological/cyclic subtree cannot loop. Matches
    /// `AccessibilitySurfaceDriveTarget`'s walk depth so flattened indices align.
    private let maxDepth: Int

    // MARK: - State

    /// The perceived optimistic-concurrency version. It starts unobserved (`0`) and
    /// bumps on every perceived change, so a re-emitted UI request's `Fingerprint`
    /// diverges exactly when the perceived UI state changed (Inv 18).
    private var version: SurfaceVersion = 0

    /// The last harvested core (everything but the derived `version`), so an emit
    /// fires only on a real perceived change and `version` advances in lock-step.
    private var lastCore: ObservedCore?

    /// Coalesces a burst of change signals into a single emit per run-loop turn.
    private var emitScheduled = false

    private var cancellables: Set<AnyCancellable> = []

    /// The local mouse-moved monitor token, removed on deinit.
    private var mouseMonitor: Any?

    // MARK: - Init

    init(socket: SurfaceSocket, root: NSView?, surface: SurfaceId = 0, maxDepth: Int = 32) {
        self.socket = socket
        self.root = root
        self.surface = surface
        self.maxDepth = maxDepth
        observeHumanEdits()
        observeAXState()
        // Defer the first snapshot so the view has a chance to enter its window;
        // window notifications re-emit once it does.
        DispatchQueue.main.async { [weak self] in self?.scheduleEmit() }
    }

    deinit {
        if let mouseMonitor { NSEvent.removeMonitor(mouseMonitor) }
    }

    // MARK: - Human edits (→ SurfaceMutated{cause: .human})

    /// Subscribe to genuine user field-editor edits. The OS posts
    /// `textDidChangeNotification` only for direct user typing, never for the
    /// programmatic `setAccessibilityValue` an agent write goes through, so this is
    /// the sole, un-double-counted `cause = Human` source (Inv 18).
    private func observeHumanEdits() {
        NotificationCenter.default.publisher(for: NSControl.textDidChangeNotification)
            .receive(on: RunLoop.main)
            .sink { [weak self] note in self?.reportHumanEdit(note) }
            .store(in: &cancellables)
    }

    /// Translate one user value edit into a `SurfaceOp.setValue` and send it as a
    /// `cause = .human` mutation, then refresh the observed snapshot so the
    /// perceived version/value follow.
    private func reportHumanEdit(_ note: Notification) {
        guard
            let root,
            let control = note.object as? NSView,
            control.isDescendant(of: root),
            let element = elementId(of: control)
        else { return }

        let value: JSONValue = .string((control as? NSControl)?.stringValue ?? "")
        let op = SurfaceOp.setValue(surface: surface, element: element, value: value, baseVersion: nil)
        socket.send(mutation: op, cause: .human)
        scheduleEmit()
    }

    // MARK: - Observed AX state (→ SurfaceObserved)

    /// Subscribe to the window/text signals that change the perceived surface
    /// state, plus a coalesced mouse-moved monitor for the cursor. Each fires a
    /// (deduped) snapshot emit.
    private func observeAXState() {
        let names: [Notification.Name] = [
            NSWindow.didBecomeKeyNotification,
            NSWindow.didResignKeyNotification,
            NSWindow.didBecomeMainNotification,
            NSWindow.didResignMainNotification,
            NSWindow.didResizeNotification,
            NSWindow.didMoveNotification,
            NSWindow.didMiniaturizeNotification,
            NSWindow.didDeminiaturizeNotification,
            NSView.boundsDidChangeNotification,
            NSControl.textDidBeginEditingNotification,
            NSControl.textDidEndEditingNotification,
            NSTextView.didChangeSelectionNotification,
        ]
        for name in names {
            NotificationCenter.default.publisher(for: name)
                .receive(on: RunLoop.main)
                .sink { [weak self] _ in self?.scheduleEmit() }
                .store(in: &cancellables)
        }

        // A cursor move is a perceived change; coalescing collapses a burst of
        // moves to one emit per run-loop turn, and a move outside the surface root
        // harvests no cursor (so it never churns the snapshot).
        mouseMonitor = NSEvent.addLocalMonitorForEvents(matching: .mouseMoved) { [weak self] event in
            self?.scheduleEmit()
            return event
        }
    }

    /// Coalesce overlapping change signals into one emit on the next run-loop turn,
    /// so a single user gesture never produces a redundant burst of snapshots.
    private func scheduleEmit() {
        guard !emitScheduled else { return }
        emitScheduled = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.emitScheduled = false
            self.emitIfChanged()
        }
    }

    /// Harvest the current perceived core and, when it differs from the last one,
    /// bump the perceived `version` and send a `SurfaceObserved` snapshot.
    private func emitIfChanged() {
        guard let core = makeCore() else { return }
        guard core != lastCore else { return }
        lastCore = core
        version &+= 1

        let observed = SurfaceObserved(
            surface: surface,
            version: version,
            axDigest: core.axDigest,
            focus: core.focus,
            selection: core.selection,
            viewport: core.viewport,
            window: core.window,
            cursor: core.cursor
        )
        socket.send(observation: observed)
    }

    // MARK: - Harvesting

    /// One harvested perceived state, sans the derived `version`. Equatable so an
    /// emit fires only on a real change and the version stays in lock-step.
    private struct ObservedCore: Equatable {
        let axDigest: Hash
        let focus: ElementId?
        let selection: Selection?
        let viewport: Viewport
        let window: WindowState
        let cursor: SurfacePoint?
    }

    /// Snapshot the perceived surface state from the AX subtree and window, or
    /// `nil` when the root has been deallocated.
    private func makeCore() -> ObservedCore? {
        guard let root else { return nil }
        let nodes = flattenedAX(root)
        return ObservedCore(
            axDigest: Hash(axDigest(of: nodes)),
            focus: focusedElementId(in: nodes),
            selection: currentSelection(),
            viewport: currentViewport(),
            window: currentWindowState(),
            cursor: currentCursor()
        )
    }

    /// The flattened AX subtree of `root` in stable depth-first order — the same
    /// order `AccessibilitySurfaceDriveTarget` indexes, so `ElementId`s align
    /// across apply and observe.
    private func flattenedAX(_ root: NSView) -> [NSAccessibilityProtocol] {
        var flat: [NSAccessibilityProtocol] = []
        var stack: [(node: NSAccessibilityProtocol, depth: Int)] = [(root, 0)]
        while let (node, depth) = stack.popLast() {
            flat.append(node)
            guard depth < maxDepth, let children = node.accessibilityChildren() else { continue }
            for child in children.reversed() {
                if let axChild = child as? NSAccessibilityProtocol {
                    stack.append((axChild, depth + 1))
                }
            }
        }
        return flat
    }

    /// The flattened index of `control`, matching the drive side's `ElementId`
    /// addressing, or `nil` when the control is not in the subtree.
    private func elementId(of control: NSView) -> ElementId? {
        guard let root else { return nil }
        let nodes = flattenedAX(root)
        for (index, node) in nodes.enumerated() where (node as? NSView) === control {
            return ElementId(index)
        }
        return nil
    }

    /// A deterministic content hash of the observed AX subtree (role / subrole /
    /// label / value per node, in flattened order). Position-independent so a mere
    /// window move does not churn it (window geometry rides `WindowState`); FNV-1a
    /// keeps it reproducible across processes, unlike Swift's randomized `Hasher`.
    private func axDigest(of nodes: [NSAccessibilityProtocol]) -> String {
        var canonical = ""
        for node in nodes {
            let role = node.accessibilityRole()?.rawValue ?? ""
            let subrole = node.accessibilitySubrole()?.rawValue ?? ""
            let label = node.accessibilityLabel() ?? ""
            let value = node.accessibilityValue().map { String(describing: $0) } ?? ""
            canonical += "\(role)|\(subrole)|\(label)|\(value)\n"
        }
        return Self.fnv1a(canonical)
    }

    /// The flattened index of the focused element, resolving a field editor back to
    /// its owning control, or `nil` when nothing in the subtree holds focus.
    private func focusedElementId(in nodes: [NSAccessibilityProtocol]) -> ElementId? {
        guard let responder = root?.window?.firstResponder else { return nil }
        let focusView: NSView?
        if let fieldEditor = responder as? NSText {
            focusView = (fieldEditor.delegate as? NSView) ?? fieldEditor.superview
        } else {
            focusView = responder as? NSView
        }
        guard let target = focusView else { return nil }
        for (index, node) in nodes.enumerated() where (node as? NSView) === target {
            return ElementId(index)
        }
        return nil
    }

    /// The selected text range of the focused field editor as a canonical
    /// `"location,length"` descriptor, or `nil` when no text is being edited.
    private func currentSelection() -> Selection? {
        guard let fieldEditor = root?.window?.firstResponder as? NSText else { return nil }
        let range = fieldEditor.selectedRange
        return Selection("\(range.location),\(range.length)")
    }

    /// The visible viewport rect (the enclosing scroll view's document-visible
    /// rect, else the root's visible rect) as a canonical descriptor.
    private func currentViewport() -> Viewport {
        guard let root else { return Viewport("0,0,0,0") }
        let rect = root.enclosingScrollView?.documentVisibleRect ?? root.visibleRect
        return Viewport(Self.canonicalRect(rect))
    }

    /// The window geometry and key/main/miniaturized/zoomed flags as a canonical
    /// descriptor, or `"none"` while the root is not yet in a window.
    private func currentWindowState() -> WindowState {
        guard let window = root?.window else { return WindowState("none") }
        let flags = [
            window.isKeyWindow ? "key" : nil,
            window.isMainWindow ? "main" : nil,
            window.isMiniaturized ? "min" : nil,
            window.isZoomed ? "zoom" : nil,
        ].compactMap { $0 }.joined(separator: "+")
        return WindowState("\(Self.canonicalRect(window.frame));\(flags.isEmpty ? "normal" : flags)")
    }

    /// The cursor as a `(column, row)` point in the surface root's coordinate
    /// space, or `nil` when the pointer is outside the surface or no window hosts
    /// it (so an off-surface move never churns the snapshot).
    private func currentCursor() -> SurfacePoint? {
        guard let root, let window = root.window else { return nil }
        let windowPoint = window.convertPoint(fromScreen: NSEvent.mouseLocation)
        let viewPoint = root.convert(windowPoint, from: nil)
        guard root.bounds.contains(viewPoint) else { return nil }
        return SurfacePoint(
            column: UInt32(max(0, viewPoint.x.rounded())),
            row: UInt32(max(0, viewPoint.y.rounded()))
        )
    }

    // MARK: - Deterministic helpers

    /// A canonical, rounded `"x,y,w,h"` rendering of a rect so the descriptor is
    /// stable for the same geometry across observations.
    private static func canonicalRect(_ rect: NSRect) -> String {
        "\(Int(rect.origin.x.rounded())),\(Int(rect.origin.y.rounded()))," +
        "\(Int(rect.size.width.rounded())),\(Int(rect.size.height.rounded()))"
    }

    /// A deterministic 64-bit FNV-1a hash of a string, rendered as 16 hex digits.
    /// Used for `ax_digest` because it reproduces across processes and replays,
    /// where Swift's per-process-randomized `Hasher` would not.
    private static func fnv1a(_ string: String) -> String {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        let prime: UInt64 = 0x0000_0100_0000_01b3
        for byte in string.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* prime
        }
        return String(format: "%016llx", hash)
    }
}
