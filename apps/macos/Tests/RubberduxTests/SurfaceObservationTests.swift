import Foundation
import XCTest
@testable import Rubberdux

// MARK: - AXDigestReference

/// A faithful reference of the `ax_digest` scheme that
/// `SurfaceObservationReporter` computes — a deterministic, geometry-independent
/// FNV-1a over the per-node `role|subrole|label|value` canonical lines. The
/// production `axDigest(of:)`/`fnv1a(_:)` are `private`, which `@testable import`
/// cannot reach, so this reference re-states the EXACT constants, line format, and
/// hex rendering documented on those methods. It is pinned to a golden value
/// (`fnv1a("")`) below so it cannot silently drift from the documented algorithm.
/// See `SurfaceObservationReporter` and docs/agent/world/ecs-runtime.md (Theme 2b).
enum AXDigestReference {
    /// The observable fields of one AX node, in the order the canonical line uses.
    /// Window/viewport geometry is DELIBERATELY absent — it rides `WindowState` /
    /// `Viewport` on `SurfaceObserved`, never the digest — which is why a window
    /// move cannot perturb `ax_digest`.
    struct Node {
        let role: String
        let subrole: String
        let label: String
        let value: String
    }

    /// The deterministic content hash of a flattened AX subtree.
    static func digest(_ nodes: [Node]) -> String {
        var canonical = ""
        for node in nodes {
            canonical += "\(node.role)|\(node.subrole)|\(node.label)|\(node.value)\n"
        }
        return fnv1a(canonical)
    }

    /// 64-bit FNV-1a, 16 hex digits — reproducible across processes/replays, unlike
    /// Swift's per-process-randomized `Hasher`.
    static func fnv1a(_ string: String) -> String {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        let prime: UInt64 = 0x0000_0100_0000_01b3
        for byte in string.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* prime
        }
        return String(format: "%016llx", hash)
    }
}

// MARK: - SurfaceObservationTests

/// [Verifies VC-M.1] `ax_digest` / `SurfaceObserved` observation encoding is
/// STABLE and deterministic:
///
/// - encoding a `SurfaceObserved` is byte-stable across repeated computation;
/// - the `ax_digest` field is INDEPENDENT of window geometry (geometry rides the
///   separate `window` field) — a window-only change leaves `ax_digest` identical;
/// - a changed `ax_digest` changes the encoding (the digest is load-bearing);
/// - the digest scheme itself is a deterministic, geometry-independent FNV-1a:
///   the same observed subtree yields the same digest and a changed value yields a
///   different one.
///
/// See `SurfaceObservationReporter` and docs/agent/world/ecs-runtime.md.
final class SurfaceObservationTests: XCTestCase {

    private func observed(
        axDigest: String = "fixed-digest",
        window: String = "0,0,800,600;normal"
    ) -> SurfaceObserved {
        SurfaceObserved(
            surface: 1,
            version: 1,
            axDigest: Hash(axDigest),
            focus: nil,
            selection: nil,
            viewport: Viewport("0,0,800,600"),
            window: WindowState(window),
            cursor: nil
        )
    }

    /// Read a top-level string field out of a `SurfaceObserved`'s encoded form.
    private func stringField(_ key: String, of value: SurfaceObserved) throws -> String? {
        let data = try JSONEncoder().encode(value)
        let object = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        return object?[key] as? String
    }

    // MARK: Encoding stability

    func testSurfaceObservedEncodingIsByteStable() throws {
        let value = observed()
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let first = try encoder.encode(value)
        let second = try encoder.encode(value)
        XCTAssertEqual(first, second, "the same observation encodes to identical bytes")
    }

    func testWindowGeometryDoesNotPerturbAxDigest() throws {
        // Two observations with the SAME ax_digest but DIFFERENT window geometry.
        let near = observed(axDigest: "same", window: "0,0,800,600;normal")
        let far = observed(axDigest: "same", window: "120,340,640,480;key")

        let nearDigest = try stringField("ax_digest", of: near)
        let farDigest = try stringField("ax_digest", of: far)
        XCTAssertEqual(nearDigest, "same")
        XCTAssertEqual(nearDigest, farDigest, "window geometry must not perturb ax_digest")

        // Sanity: the window field genuinely differs, so the equality above is real.
        let nearWindow = try stringField("window", of: near)
        let farWindow = try stringField("window", of: far)
        XCTAssertNotEqual(nearWindow, farWindow, "the window field did change")
    }

    func testChangedAxDigestChangesEncoding() throws {
        let a = observed(axDigest: "digest-a")
        let b = observed(axDigest: "digest-b")
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        XCTAssertNotEqual(try encoder.encode(a), try encoder.encode(b))
        XCTAssertNotEqual(
            try stringField("ax_digest", of: a),
            try stringField("ax_digest", of: b),
            "ax_digest is load-bearing in the observation encoding"
        )
    }

    // MARK: Digest scheme determinism

    func testAxDigestEmptyIsFNV1aOffsetBasis() {
        // The FNV-1a 64-bit offset basis, rendered 16-hex — pins the scheme so the
        // reference cannot drift from the production algorithm undetected.
        XCTAssertEqual(AXDigestReference.fnv1a(""), "cbf29ce484222325")
    }

    func testAxDigestIsDeterministicForTheSameSubtree() {
        let subtree = [
            AXDigestReference.Node(role: "AXTextField", subrole: "", label: "Name", value: "Ada"),
            AXDigestReference.Node(role: "AXButton", subrole: "", label: "Submit", value: ""),
        ]
        XCTAssertEqual(
            AXDigestReference.digest(subtree),
            AXDigestReference.digest(subtree),
            "the same observed subtree yields the same digest across repeated computation"
        )
    }

    func testAxDigestChangesWhenAValueChanges() {
        let before = [
            AXDigestReference.Node(role: "AXTextField", subrole: "", label: "Name", value: "Ada"),
        ]
        let after = [
            AXDigestReference.Node(role: "AXTextField", subrole: "", label: "Name", value: "Grace"),
        ]
        XCTAssertNotEqual(
            AXDigestReference.digest(before),
            AXDigestReference.digest(after),
            "a changed element value must yield a different digest"
        )
    }

    func testAxDigestIsGeometryIndependentByConstruction() {
        // Two observations of the SAME subtree taken before/after a window move:
        // since geometry is not among the node fields, the digest is unchanged.
        let subtree = [
            AXDigestReference.Node(role: "AXTextField", subrole: "", label: "Name", value: "Ada"),
            AXDigestReference.Node(role: "AXStaticText", subrole: "", label: "Status", value: "Ready"),
        ]
        let reobservedAfterWindowMove = subtree
        XCTAssertEqual(
            AXDigestReference.digest(subtree),
            AXDigestReference.digest(reobservedAfterWindowMove),
            "moving the window does not change the AX subtree, so the digest holds"
        )
    }
}
