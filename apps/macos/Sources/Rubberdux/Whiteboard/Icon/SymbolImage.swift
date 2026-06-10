import AppKit

// MARK: - SymbolImage

/// Maps an SF-Symbol name string (as stored in `Icon.symbol`) to an `NSImage`.
///
/// If `NSImage(systemSymbolName:accessibilityDescription:)` returns `nil` for
/// the given name (unknown or unavailable symbol), the fallback glyph
/// `"questionmark.circle"` is used instead so the caller always receives a
/// valid image.
enum SymbolImage {
    // MARK: - Fallback glyph

    /// The SF-Symbol name used when the requested name is not available.
    static let fallbackSymbolName = "questionmark.circle"

    // MARK: - Lookup

    /// Return an `NSImage` for the SF-Symbol named `name`.
    /// Returns the fallback glyph image when `name` is not a valid symbol.
    static func image(named name: String) -> NSImage {
        if let image = NSImage(systemSymbolName: name, accessibilityDescription: nil) {
            return image
        }
        // The fallback is a built-in system symbol, so this should never be nil.
        return NSImage(
            systemSymbolName: fallbackSymbolName,
            accessibilityDescription: nil
        ) ?? NSImage()
    }
}
