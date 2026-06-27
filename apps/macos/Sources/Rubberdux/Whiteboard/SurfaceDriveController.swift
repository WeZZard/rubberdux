import AppKit
import Combine

// MARK: - SurfaceDriveTarget

/// The real-UI seam M-apply drives: a concrete client surface that can apply one
/// agent `SurfaceOp` to its actual AppKit/Accessibility elements. The whiteboard
/// provides the realization (`AccessibilitySurfaceDriveTarget`); the
/// `SurfaceDriveController` owns the protocol concerns — `Vec`-order application,
/// the optimistic-concurrency precondition, version bumping, and echo
/// cause-stamping — and delegates only the native mutation to a target.
///
/// Each method returns whether the addressed element was realized and the native
/// action performed, so the controller can log an absent element distinctly from
/// an applied one. The version bookkeeping is the controller's alone (it mirrors
/// `apply_surface_ops` in `src/agent/world/surface.rs`), independent of whether a
/// given element is currently on screen.
protocol SurfaceDriveTarget: AnyObject {
    /// Set the value of the addressed element (AppKit binding or AX `setValue`).
    func setValue(surface: SurfaceId, element: ElementId, value: JSONValue) -> Bool
    /// Synthesise an Accessibility press/click on the addressed element.
    func click(surface: SurfaceId, element: ElementId, point: SurfacePoint?) -> Bool
    /// Navigate the addressed surface to `route` (best-effort on this client).
    func navigate(surface: SurfaceId, route: Route) -> Bool
    /// Render `component` into the addressed surface region (best-effort).
    func render(surface: SurfaceId, component: ComponentSpec) -> Bool
}

// MARK: - SurfaceDriveRejection

/// One op rejected by the optimistic-concurrency precondition (Theme 2c): its
/// `base_version` no longer matched the surface's current `SurfaceVersion`, so it
/// was NOT applied. The controller turns a batch of these into the drive's tool
/// `is_error` result, mirroring `rejection_tool_result` in
/// `src/agent/world/surface.rs`. See docs/agent/world/ecs-runtime.md (Inv 18).
struct SurfaceDriveRejection: Equatable {
    let surface: SurfaceId
    let baseVersion: SurfaceVersion
    let current: SurfaceVersion

    /// The human-readable rejection line, matching the Rust executor's wording so
    /// the surfaced `is_error` reads identically on either side.
    var message: String {
        "surface \(surface): stale base_version \(baseVersion) (current \(current))"
    }
}

// MARK: - SurfaceDriveController

/// Applies inbound agent `SurfaceDrive`s to the real macOS whiteboard UI
/// (M-apply). It subscribes to `SurfaceSocket.driveSubject` and, on the main
/// thread, applies each `SurfaceOp` in `Vec` order to the actual AppKit/AX
/// elements of the addressed surface via a `SurfaceDriveTarget`.
///
/// Optimistic concurrency (Theme 2c): an op whose `base_version` no longer
/// matches the surface's current `SurfaceVersion` is REJECTED without mutating
/// the surface and surfaced as the drive's tool `is_error`; every applied change
/// BUMPS that surface's version. This mirrors `apply_surface_ops` /
/// `rejection_tool_result` in `src/agent/world/surface.rs`.
///
/// Cause stamping (Inv 18, Theme 1e): a native UI change the controller causes is
/// the screen's echo of an already-recorded agent write, so it is stamped
/// `Cause.command(cmd:key:)` from the originating drive and sent back via
/// `SurfaceSocket.send(mutation:cause:)`. The worker bridge drops `Command`-caused
/// echoes (`bridge_inbound_surface_frame` in `src/app/runtime/worker.rs`), so the
/// agent write remains the ONE fact recorded as the drive's `ToolReturned` — the
/// controller never emits a `cause = .human` `SurfaceMutated` for its own change.
/// See docs/agent/world/ecs-runtime.md.
final class SurfaceDriveController {

    // MARK: - Collaborators

    private let socket: SurfaceSocket
    private let target: SurfaceDriveTarget

    /// Reports the drive's tool `is_error` when one or more ops were rejected by
    /// the precondition. The host's `SurfaceSystem` independently resolves the
    /// slot `is_error` against its own surface-version view fed by `SurfaceObserved`
    /// (`rejection_tool_result`); this hook is the client-side surfacing seam so
    /// the rejection is observable and testable without M-observe's AX scrape. The
    /// default logs it with a clear marker.
    private let reportToolError: (CmdId, IdempotencyKey, [SurfaceDriveRejection]) -> Void

    // MARK: - State

    /// Per-surface optimistic-concurrency counter. A surface absent here is
    /// treated as version `0` (unobserved), matching `SurfaceState::unobserved`
    /// in `src/agent/world/surface.rs`.
    private var versions: [SurfaceId: SurfaceVersion] = [:]

    private var cancellables: Set<AnyCancellable> = []

    // MARK: - Init

    init(
        socket: SurfaceSocket,
        target: SurfaceDriveTarget,
        reportToolError: @escaping (CmdId, IdempotencyKey, [SurfaceDriveRejection]) -> Void = SurfaceDriveController.logToolError
    ) {
        self.socket = socket
        self.target = target
        self.reportToolError = reportToolError

        // Apply on the main thread: every op touches AppKit/AX state, and the
        // chat/UI path must never block (project UX rule). The socket publishes
        // off its own queue, so hop to main exactly as the board's other sinks do.
        socket.driveSubject
            .receive(on: RunLoop.main)
            .sink { [weak self] drive in
                self?.apply(drive)
            }
            .store(in: &cancellables)
    }

    // MARK: - Apply

    /// Apply one drive's ops in `Vec` order. Each non-rejected op bumps its
    /// surface's version and stamps a `Cause.command` echo; rejected ops are
    /// collected and surfaced as the drive's single tool `is_error`.
    private func apply(_ drive: SurfaceDrive) {
        var rejections: [SurfaceDriveRejection] = []

        for op in drive.ops {
            let surface = op.surface
            let current = versions[surface] ?? 0

            // Stale precondition — reject without clobbering the changed surface.
            if let base = op.baseVersion, base != current {
                rejections.append(
                    SurfaceDriveRejection(surface: surface, baseVersion: base, current: current)
                )
                continue
            }

            // Accepted (no precondition, or it matches): apply to the real UI and
            // bump the version. The version advances on every accepted op exactly
            // as the World's `apply_surface_ops` does, independent of whether the
            // addressed element is currently realized on screen.
            versions[surface] = current + 1
            let applied = applyToRealUI(op)
            if !applied {
                NSLog(
                    "[SurfaceDrive] op applied to version %llu but element was not realized: %@",
                    current + 1,
                    "\(op)"
                )
            }

            // Stamp the native echo with the originating command's cause so the
            // worker bridge dedups it (Inv 18) — never a `cause = .human` echo for
            // the agent's own write.
            socket.send(mutation: op, cause: .command(cmd: drive.cmd, key: drive.key))
        }

        if !rejections.isEmpty {
            reportToolError(drive.cmd, drive.key, rejections)
        }
    }

    /// Dispatch one accepted op to the real-UI target. Returns whether the native
    /// action landed on a realized element.
    private func applyToRealUI(_ op: SurfaceOp) -> Bool {
        switch op {
        case let .setValue(surface, element, value, _):
            return target.setValue(surface: surface, element: element, value: value)
        case let .click(surface, element, point, _):
            return target.click(surface: surface, element: element, point: point)
        case let .navigate(surface, route, _):
            return target.navigate(surface: surface, route: route)
        case let .render(surface, component, _):
            return target.render(surface: surface, component: component)
        }
    }

    // MARK: - Default error surfacing

    /// Default `reportToolError`: log the rejected ops with the same wording the
    /// Rust executor's `rejection_tool_result` uses, so the surfaced `is_error` is
    /// recognisable on either side.
    static func logToolError(cmd: CmdId, key: IdempotencyKey, rejections: [SurfaceDriveRejection]) {
        let detail = rejections.map(\.message).joined(separator: "; ")
        NSLog(
            "[SurfaceDrive] is_error for cmd %u (key %@): rejected stale surface ops: %@",
            cmd,
            key.rawValue,
            detail
        )
    }
}

// MARK: - AccessibilitySurfaceDriveTarget

/// The whiteboard's realization of `SurfaceDriveTarget`: it resolves an
/// agent-addressed `ElementId` to a real Accessibility element in a root view's
/// AX subtree and performs the native action there — the same elements an
/// external Accessibility client (e.g. the cua-driver system test) drives.
///
/// `SurfaceId` selects the surface root (the whiteboard hosts a single surface,
/// its container view); `ElementId` indexes the root's flattened AX subtree in a
/// stable depth-first order. `Navigate`/`Render` have no whiteboard realization
/// yet and are best-effort no-ops with a clear marker (the surface has no route
/// or component-render concept at this layer).
final class AccessibilitySurfaceDriveTarget: SurfaceDriveTarget {

    /// The surface root whose AX subtree the agent addresses. Weak so the target
    /// never keeps the view hierarchy alive past its controller.
    private weak var root: NSView?

    /// A guard on the AX walk so a pathological/cyclic subtree cannot loop.
    private let maxDepth: Int

    init(root: NSView?, maxDepth: Int = 32) {
        self.root = root
        self.maxDepth = maxDepth
    }

    // MARK: SurfaceDriveTarget

    func setValue(surface: SurfaceId, element: ElementId, value: JSONValue) -> Bool {
        guard let ax = axElement(element) else { return false }
        ax.setAccessibilityValue(Self.axValue(from: value))
        return true
    }

    func click(surface: SurfaceId, element: ElementId, point: SurfacePoint?) -> Bool {
        guard let ax = axElement(element) else { return false }
        if let point {
            // The AX press acts on the element, not a pixel; a point override is a
            // hit-test hint the whiteboard has no finer target for yet.
            NSLog("[SurfaceDrive] click point override (%u,%u) ignored by AX press", point.column, point.row)
        }
        return ax.accessibilityPerformPress()
    }

    func navigate(surface: SurfaceId, route: Route) -> Bool {
        // STUB: the whiteboard surface has no navigable route model yet.
        NSLog("[SurfaceDrive] navigate to route '%@' is a no-op (no route model)", route.rawValue)
        return false
    }

    func render(surface: SurfaceId, component: ComponentSpec) -> Bool {
        // STUB: the whiteboard surface has no component-render region yet.
        NSLog("[SurfaceDrive] render component '%@' is a no-op (no render region)", component.rawValue)
        return false
    }

    // MARK: AX resolution

    /// The Accessibility element at `element`'s index in the root's flattened AX
    /// subtree (stable depth-first), or `nil` when the index is out of range or no
    /// root is set.
    private func axElement(_ element: ElementId) -> NSAccessibilityProtocol? {
        guard let root else { return nil }
        var flat: [NSAccessibilityProtocol] = []
        var stack: [(node: NSAccessibilityProtocol, depth: Int)] = [(root, 0)]
        while let (node, depth) = stack.popLast() {
            flat.append(node)
            guard depth < maxDepth, let children = node.accessibilityChildren() else { continue }
            // Push children reversed so the pop order is the natural child order,
            // keeping the flattened index assignment stable across calls.
            for child in children.reversed() {
                if let axChild = child as? NSAccessibilityProtocol {
                    stack.append((axChild, depth + 1))
                }
            }
        }
        let index = Int(element)
        return flat.indices.contains(index) ? flat[index] : nil
    }

    /// Lower a JSON value into the Foundation value an AX value slot accepts.
    /// Structured values fall back to their JSON string form.
    private static func axValue(from value: JSONValue) -> Any? {
        switch value {
        case .null:
            return nil
        case let .bool(bool):
            return bool
        case let .int(int):
            return int
        case let .double(double):
            return double
        case let .string(string):
            return string
        case .array, .object:
            guard
                let data = try? JSONEncoder().encode(value),
                let string = String(data: data, encoding: .utf8)
            else { return nil }
            return string
        }
    }
}
