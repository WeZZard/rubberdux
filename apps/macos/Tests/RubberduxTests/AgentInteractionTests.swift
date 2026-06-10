import XCTest
@testable import Rubberdux

final class AgentInteractionTests: XCTestCase {

    private func roundTrip<T: Codable & Equatable>(_ value: T) throws -> T {
        let data = try JSONEncoder().encode(value)
        return try JSONDecoder().decode(T.self, from: data)
    }

    // MARK: - AgentInteraction round-trips

    func testApprovalRoundTrip() throws {
        let interaction = AgentInteraction.approval(
            requestId: "r1",
            appId: "app-1",
            flavor: .permission,
            prompt: "Delete file?"
        )
        let decoded = try roundTrip(interaction)
        XCTAssertEqual(interaction, decoded)
        XCTAssertEqual(interaction.requestId, "r1")
        XCTAssertEqual(interaction.appId, "app-1")
    }

    func testApprovalKindTag() throws {
        let interaction = AgentInteraction.approval(
            requestId: "r1",
            appId: "app-1",
            flavor: .plan,
            prompt: "Approve plan?"
        )
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(interaction)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "approval")
    }

    func testQuestionRoundTrip() throws {
        let interaction = AgentInteraction.question(
            requestId: "r2",
            appId: "app-2",
            text: "Which database?",
            options: [ChoiceOption(label: "pg", description: "PostgreSQL")]
        )
        XCTAssertEqual(try roundTrip(interaction), interaction)
    }

    func testQuestionKindTag() throws {
        let interaction = AgentInteraction.question(
            requestId: "r2", appId: "app-2", text: "?", options: []
        )
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(interaction)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "question")
    }

    func testChoiceRoundTrip() throws {
        let interaction = AgentInteraction.choice(
            requestId: "r3",
            appId: "app-3",
            prompt: "Pick a theme",
            options: [
                ChoiceOption(label: "light", description: "Light theme"),
                ChoiceOption(label: "dark", description: "Dark theme"),
            ]
        )
        XCTAssertEqual(try roundTrip(interaction), interaction)
    }

    func testChoiceKindTag() throws {
        let interaction = AgentInteraction.choice(
            requestId: "r3", appId: "app-3", prompt: "?", options: []
        )
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(interaction)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "choice")
    }

    func testPreviewRoundTrip() throws {
        let interaction = AgentInteraction.preview(
            requestId: "r4",
            appId: "app-4",
            prompt: "Review icon",
            artifact: PreviewArtifact(mimeType: "image/png", content: "base64data")
        )
        XCTAssertEqual(try roundTrip(interaction), interaction)
    }

    func testPreviewKindTag() throws {
        let interaction = AgentInteraction.preview(
            requestId: "r4",
            appId: "app-4",
            prompt: "?",
            artifact: PreviewArtifact(mimeType: "text/plain", content: "x")
        )
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(interaction)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "preview")
    }

    // MARK: - InteractionResponse round-trips

    func testApprovedRoundTrip() throws {
        let response = InteractionResponse.approved(requestId: "r1", flavor: .permission)
        let decoded = try roundTrip(response)
        XCTAssertEqual(response, decoded)
        XCTAssertEqual(response.requestId, "r1")
    }

    func testApprovedKindTag() throws {
        let response = InteractionResponse.approved(requestId: "r1", flavor: .plan)
        let json = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(response)
        ) as! [String: Any]
        XCTAssertEqual(json["kind"] as? String, "approved")
    }

    func testDeclinedRoundTrip() throws {
        let response = InteractionResponse.declined(
            requestId: "r1", flavor: .plan, reason: "unsafe"
        )
        XCTAssertEqual(try roundTrip(response), response)
    }

    func testAnsweredRoundTrip() throws {
        let response = InteractionResponse.answered(
            requestId: "r2", selected: 0, reply: nil
        )
        XCTAssertEqual(try roundTrip(response), response)
    }

    func testAnsweredWithReplyRoundTrip() throws {
        let response = InteractionResponse.answered(
            requestId: "r2", selected: nil, reply: "free text"
        )
        XCTAssertEqual(try roundTrip(response), response)
    }

    func testAcknowledgedRoundTrip() throws {
        let response = InteractionResponse.acknowledged(requestId: "r4")
        XCTAssertEqual(try roundTrip(response), response)
    }
}
