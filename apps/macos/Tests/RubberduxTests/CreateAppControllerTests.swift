import AppKit
import XCTest
@testable import Rubberdux

final class CreateAppControllerTests: XCTestCase {

    func testCommitWithNonEmptyTextSubmitsOnce() {
        let controller = CreateAppController()
        controller.loadView()
        guard let textField = editableTextField(in: controller.view) else {
            XCTFail("Expected editable task text field")
            return
        }

        var submitCount = 0
        var submittedTask: String?
        var cancelCount = 0
        controller.onSubmit = { task in
            submitCount += 1
            submittedTask = task
        }
        controller.onCancel = {
            cancelCount += 1
        }

        textField.stringValue = "  Build a dashboard  "
        controller.commit()
        controller.popoverDidClose(clickOutNotification())

        XCTAssertEqual(submitCount, 1)
        XCTAssertEqual(submittedTask, "Build a dashboard")
        XCTAssertEqual(cancelCount, 0)
    }

    func testClickOutCancelsOnce() {
        let controller = CreateAppController()
        controller.loadView()

        var submitCount = 0
        var cancelCount = 0
        controller.onSubmit = { _ in
            submitCount += 1
        }
        controller.onCancel = {
            cancelCount += 1
        }

        controller.popoverDidClose(clickOutNotification())

        XCTAssertEqual(submitCount, 0)
        XCTAssertEqual(cancelCount, 1)
    }

    func testEscapeThenPopoverDidCloseCancelsOnce() {
        let controller = CreateAppController()
        controller.loadView()
        guard let textField = editableTextField(in: controller.view) else {
            XCTFail("Expected editable task text field")
            return
        }

        var submitCount = 0
        var cancelCount = 0
        controller.onSubmit = { _ in
            submitCount += 1
        }
        controller.onCancel = {
            cancelCount += 1
        }

        let handled = controller.control(
            textField,
            textView: NSTextView(),
            doCommandBy: #selector(NSResponder.cancelOperation(_:))
        )
        controller.popoverDidClose(clickOutNotification())

        XCTAssertTrue(handled)
        XCTAssertEqual(submitCount, 0)
        XCTAssertEqual(cancelCount, 1)
    }

    func testReturnKeyWithNonEmptyTextSubmitsOnce() {
        let controller = CreateAppController()
        controller.loadView()
        guard let textField = editableTextField(in: controller.view) else {
            XCTFail("Expected editable task text field")
            return
        }

        var submitCount = 0
        var submittedTask: String?
        var cancelCount = 0
        controller.onSubmit = { task in
            submitCount += 1
            submittedTask = task
        }
        controller.onCancel = {
            cancelCount += 1
        }

        textField.stringValue = "  Build a dashboard  "
        let handled = controller.control(
            textField,
            textView: NSTextView(),
            doCommandBy: #selector(NSResponder.insertNewline(_:))
        )

        XCTAssertTrue(handled)
        XCTAssertEqual(submitCount, 1)
        XCTAssertEqual(submittedTask, "Build a dashboard")
        XCTAssertEqual(cancelCount, 0)
    }

    func testDetachToWindowCloseDoesNotCancel() {
        let controller = CreateAppController()
        controller.loadView()

        var submitCount = 0
        var cancelCount = 0
        controller.onSubmit = { _ in
            submitCount += 1
        }
        controller.onCancel = {
            cancelCount += 1
        }

        controller.popoverDidClose(detachToWindowNotification())

        XCTAssertEqual(submitCount, 0)
        XCTAssertEqual(cancelCount, 0)
    }

    func testEmptyTextCommitDoesNotResolve() {
        let controller = CreateAppController()
        controller.loadView()
        guard let textField = editableTextField(in: controller.view) else {
            XCTFail("Expected editable task text field")
            return
        }

        var submitCount = 0
        var submittedTask: String?
        var cancelCount = 0
        controller.onSubmit = { task in
            submitCount += 1
            submittedTask = task
        }
        controller.onCancel = {
            cancelCount += 1
        }

        textField.stringValue = " \n\t "
        controller.commit()

        XCTAssertEqual(submitCount, 0)
        XCTAssertNil(submittedTask)
        XCTAssertEqual(cancelCount, 0)

        textField.stringValue = "Build later"
        controller.commit()

        XCTAssertEqual(submitCount, 1)
        XCTAssertEqual(submittedTask, "Build later")
        XCTAssertEqual(cancelCount, 0)
    }

    private func editableTextField(in view: NSView) -> NSTextField? {
        if let textField = view as? NSTextField, textField.isEditable {
            return textField
        }
        for subview in view.subviews {
            if let textField = editableTextField(in: subview) {
                return textField
            }
        }
        return nil
    }

    private func clickOutNotification() -> Notification {
        Notification(name: NSPopover.didCloseNotification, object: nil, userInfo: nil)
    }

    private func detachToWindowNotification() -> Notification {
        Notification(
            name: NSPopover.didCloseNotification,
            object: nil,
            userInfo: [NSPopover.closeReasonUserInfoKey: NSPopover.CloseReason.detachToWindow]
        )
    }
}
