import XCTest
@testable import Rubberdux

// MARK: - Canonical-JSON parity helpers

/// Re-render a JSON document into a canonical byte form: every object's keys are
/// sorted and all insignificant whitespace is dropped. Both the Swift-encoded
/// bytes and the fixed Rust-shape fixture pass through the SAME `JSONSerialization`
/// pipeline, so number formatting and string escaping are identical on both sides
/// and only the *structure* (which keys exist, their values, array shapes, and
/// explicit `null`s) is compared. This is the "canonical key handling" the
/// VC-M.1 parity assertion is defined against: a dropped key, an extra key, a
/// missing `null`, a wrong tag, or a point encoded as an object instead of an
/// array all survive canonicalization as a real difference.
func surfaceCanonicalJSON(_ data: Data) throws -> String {
    let object = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
    let canonical = try JSONSerialization.data(
        withJSONObject: object,
        options: [.sortedKeys, .fragmentsAllowed]
    )
    return String(decoding: canonical, as: UTF8.self)
}

/// Assert Swift↔Rust serde PARITY for one surface value against a fixed
/// Rust-serde-shaped fixture (VC-M.1):
///
/// - (i) the Swift `encode` of `value` canonicalizes byte-equal to the fixture;
/// - the fixture DECODES to a value equal to `value`;
/// - (ii) re-encoding that decoded value canonicalizes back to the fixture
///   (round-trip stability).
func assertSurfaceSerdeParity<T: Codable & Equatable>(
    _ value: T,
    matches fixture: String,
    file: StaticString = #filePath,
    line: UInt = #line
) {
    let encoder = JSONEncoder()
    let decoder = JSONDecoder()
    let fixtureData = Data(fixture.utf8)
    do {
        // (i) Swift encode == Rust-shape fixture (after canonical key handling).
        let encoded = try encoder.encode(value)
        XCTAssertEqual(
            try surfaceCanonicalJSON(encoded),
            try surfaceCanonicalJSON(fixtureData),
            "Swift encode diverged from the Rust serde shape",
            file: file,
            line: line
        )
        // Decoding the fixture yields the expected value (Rust shape → Swift value).
        let decoded = try decoder.decode(T.self, from: fixtureData)
        XCTAssertEqual(decoded, value, "decoding the Rust fixture diverged", file: file, line: line)
        // (ii) Round-trip: re-encoding the decoded value reproduces the fixture.
        let reencoded = try encoder.encode(decoded)
        XCTAssertEqual(
            try surfaceCanonicalJSON(reencoded),
            try surfaceCanonicalJSON(fixtureData),
            "re-encoding the decoded value diverged from the Rust shape",
            file: file,
            line: line
        )
    } catch {
        XCTFail("serde parity threw: \(error)", file: file, line: line)
    }
}

// MARK: - SurfaceSerdeParityTests

/// [Verifies VC-M.1] Swift↔Rust serde PARITY for the surface frames and payload
/// types. Each fixture is the EXACT JSON `serde_json` emits for the corresponding
/// Rust value in `src/protocol.rs` / `src/agent/world/surface.rs`:
///
/// - `SurfaceOp` / `Cause` are internally tagged (`{"op":…}` / `{"cause":…}`),
///   tag-first, `snake_case` variant names;
/// - `Point` and `SurfacePoint` encode as the two-element array `[column,row]`;
/// - the string newtypes (`Route`, `ComponentSpec`, `Hash`, `Selection`,
///   `Viewport`, `WindowState`, `IdempotencyKey`, `PeerEnvelopeId`) encode
///   transparently as bare JSON strings;
/// - absent `Option` fields encode as explicit `null` (no `skip_serializing_if`);
/// - the wire frames are externally tagged (`{"SurfaceDrive":{…}}`).
///
/// See docs/agent/world/ecs-runtime.md.
final class SurfaceSerdeParityTests: XCTestCase {

    // MARK: SurfaceOp — all four variants, base_version null and non-null

    func testSetValueWithBaseVersion() {
        assertSurfaceSerdeParity(
            SurfaceOp.setValue(surface: 1, element: 42, value: .string("hello"), baseVersion: 7),
            matches: #"{"op":"set_value","surface":1,"element":42,"value":"hello","base_version":7}"#
        )
    }

    func testSetValueWithNullBaseVersionAndStructuredValue() {
        assertSurfaceSerdeParity(
            SurfaceOp.setValue(
                surface: 1,
                element: 42,
                value: .object(["count": .int(3)]),
                baseVersion: nil
            ),
            matches: #"{"op":"set_value","surface":1,"element":42,"value":{"count":3},"base_version":null}"#
        )
    }

    func testClickWithPointAndNullBaseVersion() {
        assertSurfaceSerdeParity(
            SurfaceOp.click(
                surface: 2,
                element: 10,
                point: SurfacePoint(column: 100, row: 200),
                baseVersion: nil
            ),
            matches: #"{"op":"click","surface":2,"element":10,"point":[100,200],"base_version":null}"#
        )
    }

    func testClickWithNullPointAndBaseVersion() {
        assertSurfaceSerdeParity(
            SurfaceOp.click(surface: 2, element: 10, point: nil, baseVersion: 3),
            matches: #"{"op":"click","surface":2,"element":10,"point":null,"base_version":3}"#
        )
    }

    func testNavigateWithNullBaseVersion() {
        assertSurfaceSerdeParity(
            SurfaceOp.navigate(surface: 3, route: Route("home"), baseVersion: nil),
            matches: #"{"op":"navigate","surface":3,"route":"home","base_version":null}"#
        )
    }

    func testNavigateWithBaseVersion() {
        assertSurfaceSerdeParity(
            SurfaceOp.navigate(surface: 3, route: Route("settings"), baseVersion: 5),
            matches: #"{"op":"navigate","surface":3,"route":"settings","base_version":5}"#
        )
    }

    func testRenderWithBaseVersion() {
        assertSurfaceSerdeParity(
            SurfaceOp.render(surface: 4, component: ComponentSpec("button-bar"), baseVersion: 1),
            matches: #"{"op":"render","surface":4,"component":"button-bar","base_version":1}"#
        )
    }

    func testRenderWithNullBaseVersion() {
        assertSurfaceSerdeParity(
            SurfaceOp.render(surface: 4, component: ComponentSpec("x"), baseVersion: nil),
            matches: #"{"op":"render","surface":4,"component":"x","base_version":null}"#
        )
    }

    // MARK: Cause — all three variants

    func testCauseHuman() {
        assertSurfaceSerdeParity(Cause.human, matches: #"{"cause":"human"}"#)
    }

    func testCauseCommand() {
        assertSurfaceSerdeParity(
            Cause.command(cmd: 7, key: IdempotencyKey("tick-7-effect-0")),
            matches: #"{"cause":"command","cmd":7,"key":"tick-7-effect-0"}"#
        )
    }

    func testCausePeer() {
        assertSurfaceSerdeParity(
            Cause.peer(envelope: PeerEnvelopeId("env-abc-123")),
            matches: #"{"cause":"peer","envelope":"env-abc-123"}"#
        )
    }

    // MARK: SurfaceObserved — optionals present and nil

    func testSurfaceObservedAllPresent() {
        assertSurfaceSerdeParity(
            SurfaceObserved(
                surface: 1,
                version: 42,
                axDigest: Hash("digest-abc"),
                focus: 7,
                selection: Selection("sel-1"),
                viewport: Viewport("vp-1"),
                window: WindowState("ws-1"),
                cursor: SurfacePoint(column: 100, row: 200)
            ),
            matches: """
            {"surface":1,"version":42,"ax_digest":"digest-abc","focus":7,\
            "selection":"sel-1","viewport":"vp-1","window":"ws-1","cursor":[100,200]}
            """
        )
    }

    func testSurfaceObservedOptionalsNil() {
        assertSurfaceSerdeParity(
            SurfaceObserved(
                surface: 3,
                version: 11,
                axDigest: Hash("digest-xyz"),
                focus: nil,
                selection: nil,
                viewport: Viewport("rect"),
                window: WindowState("key"),
                cursor: nil
            ),
            matches: """
            {"surface":3,"version":11,"ax_digest":"digest-xyz","focus":null,\
            "selection":null,"viewport":"rect","window":"key","cursor":null}
            """
        )
    }

    // MARK: Frames — SurfaceDrive (host→app), SurfaceObservation / SurfaceMutated (app→host)

    func testSurfaceDriveFrame() {
        let drive = SurfaceDrive(
            ops: [
                .setValue(surface: 1, element: 3, value: .int(42), baseVersion: 7),
                .navigate(surface: 1, route: Route("settings"), baseVersion: nil),
            ],
            cmd: 99,
            key: IdempotencyKey("tick-99-effect-0")
        )
        assertSurfaceSerdeParity(
            HostToAgentFrame.surfaceDrive(drive),
            matches: """
            {"SurfaceDrive":{"ops":[\
            {"op":"set_value","surface":1,"element":3,"value":42,"base_version":7},\
            {"op":"navigate","surface":1,"route":"settings","base_version":null}\
            ],"cmd":99,"key":"tick-99-effect-0"}}
            """
        )
    }

    func testHelloRegistrationFrame() {
        // The registration handshake is externally tagged with a single `app_id`
        // field, matching `AgentToHost::Hello { app_id }` in `src/protocol.rs`.
        assertSurfaceSerdeParity(
            AgentToHostFrame.hello(appId: "2026-06-10-00-00-00-UTC"),
            matches: """
            {"Hello":{"app_id":"2026-06-10-00-00-00-UTC"}}
            """
        )
    }

    func testSurfaceObservationFrame() {
        let observed = SurfaceObserved(
            surface: 1,
            version: 42,
            axDigest: Hash("digest-abc"),
            focus: 7,
            selection: Selection("sel-1"),
            viewport: Viewport("vp-1"),
            window: WindowState("ws-1"),
            cursor: SurfacePoint(column: 100, row: 200)
        )
        assertSurfaceSerdeParity(
            AgentToHostFrame.surfaceObservation(observed),
            matches: """
            {"SurfaceObservation":{"observed":{"surface":1,"version":42,\
            "ax_digest":"digest-abc","focus":7,"selection":"sel-1","viewport":"vp-1",\
            "window":"ws-1","cursor":[100,200]}}}
            """
        )
    }

    func testSurfaceMutatedFrameHumanCause() {
        assertSurfaceSerdeParity(
            AgentToHostFrame.surfaceMutated(
                op: .setValue(surface: 2, element: 5, value: .string("hello"), baseVersion: 3),
                cause: .human
            ),
            matches: """
            {"SurfaceMutated":{"op":{"op":"set_value","surface":2,"element":5,\
            "value":"hello","base_version":3},"cause":{"cause":"human"}}}
            """
        )
    }

    func testSurfaceMutatedFramePeerCause() {
        assertSurfaceSerdeParity(
            AgentToHostFrame.surfaceMutated(
                op: .click(surface: 3, element: 9, point: nil, baseVersion: nil),
                cause: .peer(envelope: PeerEnvelopeId("env-xyz"))
            ),
            matches: """
            {"SurfaceMutated":{"op":{"op":"click","surface":3,"element":9,\
            "point":null,"base_version":null},"cause":{"cause":"peer","envelope":"env-xyz"}}}
            """
        )
    }
}
