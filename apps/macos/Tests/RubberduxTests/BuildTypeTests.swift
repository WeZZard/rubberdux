import XCTest
@testable import Rubberdux

final class BuildTypeTests: XCTestCase {
    func testDebugBuildType() {
        XCTAssertEqual(BuildType.debug, BuildType.debug)
    }

    func testReleaseBuildType() {
        XCTAssertEqual(BuildType.release, BuildType.release)
    }

    func testDistributeBuildType() {
        XCTAssertEqual(BuildType.distribute, BuildType.distribute)
    }
}
