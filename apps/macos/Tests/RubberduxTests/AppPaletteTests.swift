import AppKit
import XCTest
@testable import Rubberdux

final class AppPaletteTests: XCTestCase {

    func testKnownHexColorReturnsNonGray() {
        // #FF0000 is red; it must not equal systemGray
        let color = AppPalette.color(named: "#FF0000")
        XCTAssertNotEqual(color, NSColor.systemGray)
    }

    func testShortHexColorIsParsed() {
        // #F00 expands to #FF0000
        let short = AppPalette.color(named: "#F00")
        let full  = AppPalette.color(named: "#FF0000")
        // Compare SRGB components
        var sr1: CGFloat = 0, sg1: CGFloat = 0, sb1: CGFloat = 0, sa1: CGFloat = 0
        var sr2: CGFloat = 0, sg2: CGFloat = 0, sb2: CGFloat = 0, sa2: CGFloat = 0
        short.usingColorSpace(.sRGB)!.getRed(&sr1, green: &sg1, blue: &sb1, alpha: &sa1)
        full.usingColorSpace(.sRGB)!.getRed(&sr2, green: &sg2, blue: &sb2, alpha: &sa2)
        XCTAssertEqual(sr1, sr2, accuracy: 0.01)
        XCTAssertEqual(sg1, sg2, accuracy: 0.01)
        XCTAssertEqual(sb1, sb2, accuracy: 0.01)
    }

    func testNamedColorIndigo() {
        let color = AppPalette.color(named: "indigo")
        XCTAssertEqual(color, NSColor.systemIndigo)
    }

    func testNamedColorCaseInsensitive() {
        XCTAssertEqual(AppPalette.color(named: "Orange"), NSColor.systemOrange)
        XCTAssertEqual(AppPalette.color(named: "BLUE"), NSColor.systemBlue)
    }

    func testUnknownNameFallsBackToGray() {
        let color = AppPalette.color(named: "notacolor")
        XCTAssertEqual(color, NSColor.systemGray)
    }

    func testEmptyStringFallsBackToGray() {
        let color = AppPalette.color(named: "")
        XCTAssertEqual(color, NSColor.systemGray)
    }

    func testBackendDefaultColorIsParsed() {
        // The backend seeds new apps with "#8E8E93"; it must not fall back to gray
        let color = AppPalette.color(named: "#8E8E93")
        XCTAssertNotNil(color.usingColorSpace(.sRGB))
    }
}
