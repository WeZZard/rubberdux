import AppKit

class EntryRowView: NSTableRowView {
    override func drawSelection(in dirtyRect: NSRect) {
        if selectionHighlightStyle != .none {
            NSColor.controlAccentColor.withAlphaComponent(0.15).setFill()
            bounds.fill()
        }
    }
}
