import AppKit

// MARK: - AppPalette

/// Maps a color name string (as stored in `Icon.color`) to an `NSColor`.
///
/// The color name is the hex string produced by the backend's identity
/// derivation (e.g. `"#8E8E93"`). Named colors from the design vocabulary
/// (`"indigo"`, `"orange"`, etc.) are also recognised for forward compatibility.
/// Unknown names fall back to `NSColor.systemGray`.
enum AppPalette {
    // MARK: - Lookup

    /// Return the `NSColor` for `name`. Falls back to `NSColor.systemGray` for
    /// unknown names.
    static func color(named name: String) -> NSColor {
        let trimmed = name.trimmingCharacters(in: .whitespaces)

        if trimmed.hasPrefix("#") {
            if let parsed = NSColor(hexString: trimmed) {
                return parsed
            }
        }

        if let named = namedColor(trimmed) {
            return named
        }

        return .systemGray
    }

    // MARK: - Private

    private static func namedColor(_ name: String) -> NSColor? {
        switch name.lowercased() {
        case "red":         return .systemRed
        case "orange":      return .systemOrange
        case "yellow":      return .systemYellow
        case "green":       return .systemGreen
        case "mint":        return .systemMint
        case "teal":        return .systemTeal
        case "cyan":        return .systemCyan
        case "blue":        return .systemBlue
        case "indigo":      return .systemIndigo
        case "purple":      return .systemPurple
        case "pink":        return .systemPink
        case "brown":       return .systemBrown
        case "gray", "grey": return .systemGray
        default:            return nil
        }
    }
}

// MARK: - NSColor hex initializer

private extension NSColor {
    /// Initialize from a CSS-style hex string (`"#RGB"`, `"#RRGGBB"`,
    /// `"#RRGGBBAA"`). Returns `nil` for strings that do not match these forms.
    convenience init?(hexString: String) {
        var hex = hexString
        if hex.hasPrefix("#") {
            hex = String(hex.dropFirst())
        }

        let length = hex.count
        guard length == 3 || length == 6 || length == 8 else { return nil }

        var value: UInt64 = 0
        guard Scanner(string: hex).scanHexInt64(&value) else { return nil }

        let r, g, b, a: CGFloat
        switch length {
        case 3:
            let rv = (value >> 8) & 0xF
            let gv = (value >> 4) & 0xF
            let bv =  value       & 0xF
            r = CGFloat(rv | (rv << 4)) / 255
            g = CGFloat(gv | (gv << 4)) / 255
            b = CGFloat(bv | (bv << 4)) / 255
            a = 1
        case 6:
            r = CGFloat((value >> 16) & 0xFF) / 255
            g = CGFloat((value >>  8) & 0xFF) / 255
            b = CGFloat( value        & 0xFF) / 255
            a = 1
        case 8:
            r = CGFloat((value >> 24) & 0xFF) / 255
            g = CGFloat((value >> 16) & 0xFF) / 255
            b = CGFloat((value >>  8) & 0xFF) / 255
            a = CGFloat( value        & 0xFF) / 255
        default:
            return nil
        }

        self.init(srgbRed: r, green: g, blue: b, alpha: a)
    }
}
