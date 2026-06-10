import AppKit

// MARK: - InteractionPopoverController

/// Presents a single agent interaction as an `NSPopover` anchored to an App
/// icon's rect on the board. It is used when the board window is front: the
/// human sees the interaction inline next to the icon that raised it, rather
/// than as a badge.
///
/// The controller owns one `NSPopover`; presenting a new interaction while one
/// is shown closes the previous popover first, so an icon never carries two
/// stacked popovers. Anchoring uses a board-coordinate rect supplied by
/// `BoardView.iconRect(forAppID:)`, which is already edge-clamped to the board's
/// visible bounds.
///
/// See `docs/apps/macos/whiteboard-interaction.md` for the interaction-surface
/// design record.
final class InteractionPopoverController {

    private let popover = NSPopover()

    /// The request id currently shown, so the owner can dismiss the popover when
    /// that interaction resolves out from under it.
    private(set) var shownRequestId: String?

    init() {
        popover.behavior = .transient
    }

    /// Present `interaction` anchored to `rect` within `boardView`. `onRespond`
    /// is forwarded the human's answer; the owner delivers it to the backend and
    /// dismisses the popover. The preferred edge is `.maxX` (to the icon's
    /// trailing side); AppKit re-points it automatically when that side is off the
    /// screen.
    func present(
        _ interaction: AgentInteraction,
        relativeTo rect: NSRect,
        of boardView: NSView,
        onRespond: @escaping InteractionResponder
    ) {
        if popover.isShown {
            popover.performClose(nil)
        }
        popover.contentViewController = InteractionContentViewController.make(
            for: interaction,
            onRespond: onRespond
        )
        shownRequestId = interaction.requestId
        popover.show(relativeTo: rect, of: boardView, preferredEdge: .maxX)
    }

    /// Dismiss the popover if it is currently showing `requestId` (or
    /// unconditionally when `requestId` is `nil`).
    func dismiss(ifShowing requestId: String? = nil) {
        guard popover.isShown else { return }
        if let requestId, shownRequestId != requestId { return }
        popover.performClose(nil)
        shownRequestId = nil
    }
}
