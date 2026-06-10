import AppKit
import XCTest
@testable import Rubberdux

final class InteractionContentViewControllerTests: XCTestCase {

    func testApprovalMapsToApprovalController() {
        let interaction = AgentInteraction.approval(
            requestId: "r", appId: "a", flavor: .permission, prompt: "ok?"
        )
        let vc = InteractionContentViewController.make(for: interaction) { _ in }
        XCTAssertTrue(vc is ApprovalInteractionViewController)
    }

    func testQuestionMapsToQuestionController() {
        let interaction = AgentInteraction.question(
            requestId: "r", appId: "a", text: "which?",
            options: [ChoiceOption(label: "x", description: "")]
        )
        let vc = InteractionContentViewController.make(for: interaction) { _ in }
        XCTAssertTrue(vc is QuestionInteractionViewController)
    }

    func testChoiceMapsToChoiceController() {
        let interaction = AgentInteraction.choice(
            requestId: "r", appId: "a", prompt: "pick",
            options: [ChoiceOption(label: "x", description: "")]
        )
        let vc = InteractionContentViewController.make(for: interaction) { _ in }
        XCTAssertTrue(vc is ChoiceInteractionViewController)
    }

    func testPreviewMapsToPreviewController() {
        let interaction = AgentInteraction.preview(
            requestId: "r", appId: "a", prompt: "review",
            artifact: PreviewArtifact(mimeType: "text/plain", content: "hi")
        )
        let vc = InteractionContentViewController.make(for: interaction) { _ in }
        XCTAssertTrue(vc is PreviewInteractionViewController)
    }

    func testApprovalApproveProducesApprovedResponse() {
        let interaction = AgentInteraction.approval(
            requestId: "r1", appId: "a", flavor: .plan, prompt: "ok?"
        )
        var captured: InteractionResponse?
        let vc = InteractionContentViewController.make(for: interaction) { captured = $0 }
        vc.loadView()
        // Trigger the approve action via the controller's selector.
        vc.perform(Selector(("approveTapped")))
        XCTAssertEqual(captured, .approved(requestId: "r1", flavor: .plan))
    }

    func testPreviewAcknowledgeProducesAcknowledgedResponse() {
        let interaction = AgentInteraction.preview(
            requestId: "r2", appId: "a", prompt: "review",
            artifact: PreviewArtifact(mimeType: "text/plain", content: "hi")
        )
        var captured: InteractionResponse?
        let vc = InteractionContentViewController.make(for: interaction) { captured = $0 }
        vc.loadView()
        vc.perform(Selector(("acknowledgeTapped")))
        XCTAssertEqual(captured, .acknowledged(requestId: "r2"))
    }
}
