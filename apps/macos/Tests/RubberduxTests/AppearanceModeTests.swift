import XCTest
@testable import Rubberdux

final class AppearanceModeTests: XCTestCase {
    func testDefaultModeIsDockAndMenuBar() {
        XCTAssertEqual(AppearanceMode(rawValue: 0), .dockAndMenuBar)
    }

    func testActivationPolicyMapping() {
        XCTAssertEqual(AppearanceMode.dockAndMenuBar.activationPolicy, .regular)
        XCTAssertEqual(AppearanceMode.dockOnly.activationPolicy, .regular)
        XCTAssertEqual(AppearanceMode.menuBarOnly.activationPolicy, .accessory)
    }

    func testShowsStatusItemMapping() {
        XCTAssertTrue(AppearanceMode.dockAndMenuBar.showsStatusItem)
        XCTAssertTrue(AppearanceMode.menuBarOnly.showsStatusItem)
        XCTAssertFalse(AppearanceMode.dockOnly.showsStatusItem)
    }

    func testAllCasesHaveDescriptions() {
        for mode in AppearanceMode.allCases {
            XCTAssertFalse(mode.description.isEmpty)
        }
    }
}
