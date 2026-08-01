import Foundation
import XCTest
@testable import PreviewHarness

@MainActor
final class PreviewSurfaceTests: XCTestCase {
    func testSecondAttachReplacesFirstAndTracksPresentation() async throws {
        let controller = RecordingPreviewController()
        let model = PreviewSurfaceModel(controller: controller)
        let first = try makeConfiguration(sessionID: "first", presentation: "mini")
        let second = try makeConfiguration(sessionID: "second", presentation: "expanded")

        try await model.open(first)
        try await model.open(second)

        XCTAssertEqual(controller.attachedSessionIDs, ["first", "second"])
        XCTAssertEqual(controller.detachCount, 2)
        XCTAssertEqual(model.configuration?.session.sessionID, "second")
        XCTAssertEqual(model.presentation, .expanded)
        XCTAssertEqual(model.state, .playing)
    }

    func testPresentationChangeIsForwarded() async throws {
        let controller = RecordingPreviewController()
        let model = PreviewSurfaceModel(controller: controller)
        try await model.open(makeConfiguration(sessionID: "first", presentation: "mini"))

        model.setPresentation(.expanded)

        XCTAssertEqual(model.presentation, .expanded)
        XCTAssertEqual(controller.presentations, [.mini, .expanded])
    }

    func testCloseDetachesAndClearsEphemeralState() async throws {
        let controller = RecordingPreviewController()
        let model = PreviewSurfaceModel(controller: controller)
        try await model.open(makeConfiguration(sessionID: "first", presentation: "mini"))

        model.close()

        XCTAssertEqual(model.state, .idle)
        XCTAssertNil(model.configuration)
        XCTAssertNil(model.metrics)
        XCTAssertEqual(controller.detachCount, 2)
    }

    func testFailedAttachDoesNotLeaveDescriptorAttached() async throws {
        let controller = RecordingPreviewController()
        controller.attachError = TestError.failed
        let model = PreviewSurfaceModel(controller: controller)

        do {
            try await model.open(makeConfiguration(sessionID: "first", presentation: "mini"))
            XCTFail("Expected attach to fail")
        } catch {
            XCTAssertNil(model.configuration)
            guard case .failed = model.state else {
                return XCTFail("Expected failed state")
            }
            XCTAssertEqual(controller.detachCount, 2)
        }
    }

    func testNavigationPolicyStaysOnExactLoopbackOrigin() throws {
        let origin = try XCTUnwrap(URL(string: "http://127.0.0.1:7331/"))

        XCTAssertTrue(
            WKWebRTCPreviewSurface.isAllowedNavigation(
                try XCTUnwrap(URL(string: "http://127.0.0.1:7331/")),
                for: origin
            )
        )
        XCTAssertFalse(
            WKWebRTCPreviewSurface.isAllowedNavigation(
                try XCTUnwrap(URL(string: "http://127.0.0.1:7331/#attach=abc")),
                for: origin
            )
        )
        XCTAssertFalse(
            WKWebRTCPreviewSurface.isAllowedNavigation(
                try XCTUnwrap(URL(string: "http://127.0.0.1:7331/fixtures/clock-game.html")),
                for: origin
            )
        )
        XCTAssertFalse(
            WKWebRTCPreviewSurface.isAllowedNavigation(
                try XCTUnwrap(URL(string: "http://127.0.0.1:7332/")),
                for: origin
            )
        )
        XCTAssertFalse(
            WKWebRTCPreviewSurface.isAllowedNavigation(
                try XCTUnwrap(URL(string: "https://127.0.0.1:7331/")),
                for: origin
            )
        )
        XCTAssertFalse(
            WKWebRTCPreviewSurface.isAllowedNavigation(
                try XCTUnwrap(URL(string: "http://example.com:7331/")),
                for: origin
            )
        )
        XCTAssertFalse(
            WKWebRTCPreviewSurface.isAllowedNavigation(
                try XCTUnwrap(URL(string: "http://127.0.0.1:7331/?token=secret")),
                for: origin
            )
        )
    }

    func testPendingNavigationRequiresExactObjectAndAttachmentEpoch() {
        var tracker = WKPreviewPendingNavigationTracker<String>()
        let navigation = NSObject()
        let unrelatedNavigation = NSObject()
        let requestID = UUID()

        XCTAssertNil(
            tracker.install(
                navigation: navigation,
                attachmentEpoch: 7,
                requestID: requestID,
                value: "pending"
            )
        )
        XCTAssertNil(
            tracker.take(
                matching: unrelatedNavigation,
                attachmentEpoch: 7
            )
        )
        XCTAssertNil(
            tracker.take(
                matching: navigation,
                attachmentEpoch: 8
            )
        )
        XCTAssertTrue(tracker.hasPending)
        XCTAssertEqual(
            tracker.take(
                matching: navigation,
                attachmentEpoch: 7
            ),
            "pending"
        )
        XCTAssertFalse(tracker.hasPending)
        XCTAssertNil(
            tracker.take(
                matching: navigation,
                attachmentEpoch: 7
            )
        )
    }

    func testStaleNavigationCallbackCannotResolveReplacement() {
        var tracker = WKPreviewPendingNavigationTracker<String>()
        let staleNavigation = NSObject()
        let currentNavigation = NSObject()

        XCTAssertNil(
            tracker.install(
                navigation: staleNavigation,
                attachmentEpoch: 11,
                requestID: UUID(),
                value: "stale"
            )
        )
        XCTAssertEqual(
            tracker.install(
                navigation: currentNavigation,
                attachmentEpoch: 12,
                requestID: UUID(),
                value: "current"
            ),
            "stale"
        )
        XCTAssertNil(
            tracker.take(
                matching: staleNavigation,
                attachmentEpoch: 12
            )
        )
        XCTAssertEqual(
            tracker.take(
                matching: currentNavigation,
                attachmentEpoch: 12
            ),
            "current"
        )
    }

    func testCancellationIsRequestScopedAndCompletesOnlyOnce() {
        var tracker = WKPreviewPendingNavigationTracker<String>()
        let activeRequestID = UUID()

        tracker.install(
            navigation: NSObject(),
            attachmentEpoch: 19,
            requestID: activeRequestID,
            value: "continuation"
        )

        XCTAssertNil(tracker.take(requestID: UUID()))
        XCTAssertEqual(
            tracker.take(requestID: activeRequestID),
            "continuation"
        )
        XCTAssertNil(tracker.take(requestID: activeRequestID))
        XCTAssertNil(tracker.takeAny())

        tracker.install(
            navigation: NSObject(),
            attachmentEpoch: 20,
            requestID: UUID(),
            value: "detached"
        )
        XCTAssertEqual(tracker.takeAny(), "detached")
        XCTAssertNil(tracker.takeAny())
    }

    func testNativeBridgeParserAcceptsOnlyItsShallowExactSchema() {
        XCTAssertEqual(
            WKPreviewBridgePayload.parse([
                "type": "state",
                "state": "playing",
                "message": "ready",
            ]),
            .state("playing")
        )
        XCTAssertNil(
            WKPreviewBridgePayload.parse([
                "type": "state",
                "state": "playing",
                "nested": ["payload": String(repeating: "x", count: 32_000)],
            ])
        )
        XCTAssertNil(
            WKPreviewBridgePayload.parse([
                "type": "state",
                "state": "playing",
                "message": String(repeating: "x", count: 513),
            ])
        )

        let metrics = PreviewMetrics(
            presentedFPS: 23.8,
            width: 960,
            height: 540,
            latencyMilliseconds: 58,
            profile: "mini"
        )
        XCTAssertEqual(
            WKPreviewBridgePayload.parse([
                "type": "metrics",
                "metrics": [
                    "presentedFps": 23.8,
                    "width": 960,
                    "height": 540,
                    "latencyMs": 58,
                    "profile": "mini",
                ],
            ]),
            .metrics(metrics)
        )
        XCTAssertNil(
            WKPreviewBridgePayload.parse([
                "type": "metrics",
                "metrics": [
                    "presentedFps": true,
                    "width": 960,
                    "height": 540,
                    "profile": "mini",
                ],
            ])
        )
        XCTAssertNil(
            WKPreviewBridgePayload.parse([
                "type": "metrics",
                "metrics": [
                    "presentedFps": 24,
                    "width": 960,
                    "height": 540,
                    "profile": "mini",
                    "unknown": [String(repeating: "x", count: 32_000)],
                ],
            ])
        )
    }

    func testNativeBridgeRateLimiterFailsClosedAndRecoversNextWindow() {
        var limiter = WKPreviewBridgeRateLimiter()
        for _ in 0 ..< WKPreviewBridgeRateLimiter.maximumMessagesPerWindow {
            XCTAssertTrue(limiter.admit(at: 100))
        }
        XCTAssertFalse(limiter.admit(at: 100.5))
        XCTAssertTrue(limiter.admit(at: 101))

        limiter.reset()
        XCTAssertTrue(limiter.admit(at: 0))
    }

    private func makeConfiguration(
        sessionID: String,
        presentation: String
    ) throws -> PreviewLaunchConfiguration {
        let payload: [String: Any] = [
            "protocolVersion": 1,
            "sessionId": sessionID,
            "generation": 1,
            "nonce": "nonce_0123456789abcdef",
            "token": "token_0123456789abcdefghijklmnopqrstuvwxyz",
            "signaling": ["url": "ws://127.0.0.1:7331/signal"],
            "profile": presentation == "expanded" ? "expanded" : "mini",
            "provider": "webrtc",
            "presentation": presentation,
        ]
        let data = try JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys])
        let encoded = data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        let url = try XCTUnwrap(URL(string: "wscrpt-preview://attach#attach=\(encoded)"))
        return try PreviewLaunchConfiguration.parse(deepLink: url)
    }
}

@MainActor
private final class RecordingPreviewController: PreviewSurfaceController {
    var state: PreviewState = .idle
    var stateDidChange: ((PreviewState) -> Void)?
    var metricsDidChange: ((PreviewMetrics) -> Void)?
    var attachedSessionIDs: [String] = []
    var presentations: [PreviewPresentation] = []
    var detachCount = 0
    var attachError: Error?

    func attach(
        _ session: PreviewSessionDescriptor,
        presentation: PreviewPresentation
    ) async throws {
        if let attachError {
            throw attachError
        }
        attachedSessionIDs.append(session.sessionID)
        presentations.append(presentation)
        state = .playing
        stateDidChange?(.playing)
    }

    func setPresentation(_ presentation: PreviewPresentation) {
        presentations.append(presentation)
    }

    func detach() {
        detachCount += 1
        state = .idle
        stateDidChange?(.idle)
    }
}

private enum TestError: Error {
    case failed
}
