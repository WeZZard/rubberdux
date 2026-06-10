import AppKit
import XCTest
@testable import Rubberdux

final class SymbolImageTests: XCTestCase {

    func testKnownSymbolReturnsImage() {
        let image = SymbolImage.image(named: "star")
        XCTAssertNotNil(image)
    }

    func testUnknownSymbolReturnsFallback() {
        // An invented name that is not a real SF Symbol
        let image = SymbolImage.image(named: "not.a.real.symbol.xyz.abc")
        // The returned image must be the fallback, which is non-nil
        XCTAssertNotNil(image)
    }

    func testFallbackSymbolItselfIsValid() {
        // The fallback name must resolve to a real system symbol
        let image = NSImage(
            systemSymbolName: SymbolImage.fallbackSymbolName,
            accessibilityDescription: nil
        )
        XCTAssertNotNil(image)
    }

    func testCalendarSymbolReturnsImage() {
        // "calendar" is the backend seed symbol; must always resolve
        let image = SymbolImage.image(named: "calendar")
        XCTAssertNotNil(image)
    }
}
