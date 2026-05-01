import XCTest
@testable import Rubberdux

final class BuildTypeTests: XCTestCase {
    func testDistributedSupportsLaunchAtLogin() {
        XCTAssertTrue(BuildType.distributed.supportsLaunchAtLogin)
    }

    func testDebugDoesNotSupportLaunchAtLogin() {
        XCTAssertFalse(BuildType.debug.supportsLaunchAtLogin)
    }

    func testReleaseDoesNotSupportLaunchAtLogin() {
        XCTAssertFalse(BuildType.release.supportsLaunchAtLogin)
    }
}
