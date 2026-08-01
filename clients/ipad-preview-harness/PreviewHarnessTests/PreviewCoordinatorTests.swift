import NIOCore
import NIOPosix
import XCTest
@testable import PreviewHarness

@MainActor
final class PreviewCoordinatorTests: XCTestCase {
    func testAttachDiscoversThenListensBeforeIssuingOneUseToken() async throws {
        let events = LockedEventRecorder()
        let ssh = RecordingPreviewSSH(events: events)
        let presenter = RecordingPreviewPresenter(events: events)
        let factory = RecordingForwardFactory(events: events, localPort: 49_152)
        let coordinator = PreviewCoordinator(
            ssh: ssh,
            commands: try RemotePreviewCommandBuilder(
                workspacePath: "~/Developer's Game"
            ),
            previewPresenter: presenter,
            forwardFactory: factory
        )

        try await coordinator.attach(
            sessionID: "session-019d",
            profile: .expandedHeadroom,
            presentation: .expanded
        )

        XCTAssertEqual(
            events.snapshot(),
            ["status", "listen", "token", "open"]
        )
        XCTAssertEqual(
            coordinator.state,
            .attached(sessionID: "session-019d", localPort: 49_152)
        )
        let tokenCommand = try XCTUnwrap(ssh.commands.last)
        XCTAssertTrue(tokenCommand.contains("--issue-token"))
        XCTAssertTrue(tokenCommand.contains("--local-port"))
        XCTAssertTrue(tokenCommand.contains("49152"))
        XCTAssertTrue(tokenCommand.contains("--expected-remote-port"))
        XCTAssertTrue(tokenCommand.contains("7331"))
        XCTAssertTrue(tokenCommand.contains("\"$HOME\"/"))

        await coordinator.detach()
        XCTAssertEqual(events.snapshot().suffix(2), ["presenter-close", "forward-close"])
        await ssh.shutdown()
    }

    func testTokenFailureClosesNewForwardWithoutOpeningPreview() async throws {
        let events = LockedEventRecorder()
        let ssh = RecordingPreviewSSH(events: events, tokenFailure: TestFailure.expected)
        let presenter = RecordingPreviewPresenter(events: events)
        let coordinator = PreviewCoordinator(
            ssh: ssh,
            commands: try RemotePreviewCommandBuilder(workspacePath: "."),
            previewPresenter: presenter,
            forwardFactory: RecordingForwardFactory(
                events: events,
                localPort: 49_152
            )
        )

        await XCTAssertThrowsErrorAsync {
            try await coordinator.attach(sessionID: "session-019d")
        }
        XCTAssertEqual(
            events.snapshot(),
            ["status", "listen", "token", "forward-close"]
        )
        await ssh.shutdown()
    }

    func testDetachClosesForwardEvenWhileListenerStartIsPending() async throws {
        let events = LockedEventRecorder()
        let ssh = RecordingPreviewSSH(events: events)
        let presenter = RecordingPreviewPresenter(events: events)
        let delayedForward = DelayedRecordingForward(
            events: events,
            localPort: 49_152
        )
        let coordinator = PreviewCoordinator(
            ssh: ssh,
            commands: try RemotePreviewCommandBuilder(workspacePath: "."),
            previewPresenter: presenter,
            forwardFactory: FixedForwardFactory(forward: delayedForward)
        )

        let attachment = Task {
            try await coordinator.attach(sessionID: "session-019d")
        }
        await waitForEvent("listen", in: events)
        await coordinator.detach()
        try await attachment.value

        XCTAssertEqual(
            events.snapshot(),
            ["status", "listen", "presenter-close", "forward-close"]
        )
        XCTAssertEqual(coordinator.state, .idle)
        await ssh.shutdown()
    }

    func testNewAttachRetiresTokenPhaseForwardBeforeMintingAgain() async throws {
        let events = LockedEventRecorder()
        let firstTokenGate = AsyncGate()
        let ssh = RecordingPreviewSSH(
            events: events,
            firstTokenGate: firstTokenGate
        )
        let presenter = RecordingPreviewPresenter(events: events)
        let coordinator = PreviewCoordinator(
            ssh: ssh,
            commands: try RemotePreviewCommandBuilder(workspacePath: "."),
            previewPresenter: presenter,
            forwardFactory: RecordingForwardFactory(
                events: events,
                localPort: 49_152
            )
        )

        let firstAttachment = Task {
            try await coordinator.attach(
                sessionID: "session-019d",
                profile: .expandedHeadroom,
                presentation: .expanded
            )
        }
        await waitForEvent("token", in: events)

        try await coordinator.attach(
            sessionID: "session-019d",
            profile: .expandedHeadroom,
            presentation: .expanded
        )
        try await firstAttachment.value

        XCTAssertTrue(firstTokenGate.wasCancelled)
        XCTAssertEqual(
            events.snapshot(),
            [
                "status", "listen", "token", "forward-close",
                "status", "listen", "token", "open",
            ]
        )
        XCTAssertEqual(
            coordinator.state,
            .attached(sessionID: "session-019d", localPort: 49_152)
        )

        await coordinator.detach()
        await ssh.shutdown()
    }

    func testRefreshPreservesAttachedStateAndDetachIdentity() async throws {
        let events = LockedEventRecorder()
        let ssh = ScriptedPreviewSSH(
            events: events,
            steps: [
                .response(
                    .status,
                    event: "status-original",
                    data: try makeStatusData(sessionID: "session-original")
                ),
                .response(
                    .token,
                    event: "token-original",
                    data: try makeTokenData(
                        sessionID: "session-original",
                        localPort: 49_152,
                        generation: 1
                    )
                ),
                .response(
                    .list,
                    event: "list",
                    data: try makeSessionListData(
                        sessionID: "session-original"
                    )
                ),
            ]
        )
        let presenter = StatefulRecordingPreviewPresenter(events: events)
        let originalForward = NamedRecordingForward(
            name: "original",
            events: events,
            localPort: 49_152
        )
        let coordinator = PreviewCoordinator(
            ssh: ssh,
            commands: try RemotePreviewCommandBuilder(workspacePath: "."),
            previewPresenter: presenter,
            forwardFactory: SequencedForwardFactory(
                forwards: [originalForward]
            )
        )

        try await coordinator.attach(sessionID: "session-original")
        let sessions = try await coordinator.refreshSessions()

        XCTAssertEqual(sessions.map(\.id), ["session-original"])
        XCTAssertEqual(
            coordinator.state,
            .attached(sessionID: "session-original", localPort: 49_152)
        )
        XCTAssertEqual(coordinator.attachedSessionID, "session-original")
        XCTAssertEqual(coordinator.attachedLocalPort, 49_152)
        XCTAssertEqual(presenter.activeSessionID, "session-original")
        XCTAssertEqual(originalForward.closeCount, 0)

        await coordinator.detach()

        XCTAssertEqual(originalForward.closeCount, 1)
        XCTAssertNil(presenter.activeSessionID)
        XCTAssertEqual(
            events.snapshot(),
            [
                "status-original", "listen-original", "token-original",
                "open-session-original", "list", "presenter-close",
                "forward-close-original",
            ]
        )
        XCTAssertEqual(ssh.remainingStepCount, 0)
        await ssh.shutdown()
    }

    func testFailedReplacementBeforePresenterOpenRetainsPriorAttachment() async throws {
        let events = LockedEventRecorder()
        let ssh = ScriptedPreviewSSH(
            events: events,
            steps: [
                .response(
                    .status,
                    event: "status-original",
                    data: try makeStatusData(sessionID: "session-original")
                ),
                .response(
                    .token,
                    event: "token-original",
                    data: try makeTokenData(
                        sessionID: "session-original",
                        localPort: 49_152,
                        generation: 1
                    )
                ),
                .response(
                    .status,
                    event: "status-replacement",
                    data: try makeStatusData(sessionID: "session-replacement")
                ),
                .failure(
                    .token,
                    event: "token-replacement",
                    error: TestFailure.expected
                ),
            ]
        )
        let presenter = StatefulRecordingPreviewPresenter(events: events)
        let originalForward = NamedRecordingForward(
            name: "original",
            events: events,
            localPort: 49_152
        )
        let replacementForward = NamedRecordingForward(
            name: "replacement",
            events: events,
            localPort: 49_152
        )
        let coordinator = PreviewCoordinator(
            ssh: ssh,
            commands: try RemotePreviewCommandBuilder(workspacePath: "."),
            previewPresenter: presenter,
            forwardFactory: SequencedForwardFactory(
                forwards: [originalForward, replacementForward]
            )
        )

        try await coordinator.attach(sessionID: "session-original")
        await XCTAssertThrowsErrorAsync {
            try await coordinator.attach(sessionID: "session-replacement")
        }

        XCTAssertEqual(
            coordinator.state,
            .attached(sessionID: "session-original", localPort: 49_152)
        )
        XCTAssertEqual(coordinator.attachedSessionID, "session-original")
        XCTAssertEqual(coordinator.attachedLocalPort, 49_152)
        XCTAssertEqual(presenter.activeSessionID, "session-original")
        XCTAssertEqual(originalForward.closeCount, 0)
        XCTAssertEqual(replacementForward.closeCount, 1)
        XCTAssertFalse(events.snapshot().contains("presenter-close"))
        XCTAssertEqual(
            events.snapshot(),
            [
                "status-original", "listen-original", "token-original",
                "open-session-original", "status-replacement",
                "listen-replacement", "token-replacement",
                "forward-close-replacement",
            ]
        )
        XCTAssertEqual(ssh.remainingStepCount, 0)

        await coordinator.detach()
        await ssh.shutdown()
    }

    func testSupersedingPresenterOpenCannotStrandStaleLivePresenter() async throws {
        let events = LockedEventRecorder()
        let openGate = AsyncGate()
        let ssh = ScriptedPreviewSSH(
            events: events,
            steps: [
                .response(
                    .status,
                    event: "status-original",
                    data: try makeStatusData(sessionID: "session-original")
                ),
                .response(
                    .token,
                    event: "token-original",
                    data: try makeTokenData(
                        sessionID: "session-original",
                        localPort: 49_152,
                        generation: 1
                    )
                ),
                .failure(
                    .status,
                    event: "status-replacement",
                    error: TestFailure.expected
                ),
            ]
        )
        let presenter = BlockingGenerationPreviewPresenter(
            events: events,
            openGate: openGate
        )
        let originalForward = NamedRecordingForward(
            name: "original",
            events: events,
            localPort: 49_152
        )
        let coordinator = PreviewCoordinator(
            ssh: ssh,
            commands: try RemotePreviewCommandBuilder(workspacePath: "."),
            previewPresenter: presenter,
            forwardFactory: SequencedForwardFactory(
                forwards: [originalForward]
            )
        )

        let originalAttachment = Task {
            try await coordinator.attach(sessionID: "session-original")
        }
        await waitForEvent("presenter-open-start-session-original", in: events)

        await XCTAssertThrowsErrorAsync {
            try await coordinator.attach(sessionID: "session-replacement")
        }
        XCTAssertEqual(originalForward.closeCount, 1)
        XCTAssertNil(presenter.activeSessionID)
        XCTAssertTrue(events.snapshot().contains("presenter-close"))

        openGate.open()
        try await originalAttachment.value

        XCTAssertNil(presenter.activeSessionID)
        if case .failed = coordinator.state {
            // Expected: the superseding status read failed after invalidating
            // the older in-flight presentation.
        } else {
            XCTFail("Expected failed state, got \(coordinator.state)")
        }
        XCTAssertEqual(
            events.snapshot(),
            [
                "status-original", "listen-original", "token-original",
                "presenter-open-start-session-original", "presenter-close",
                "forward-close-original", "status-replacement",
                "presenter-open-discarded-session-original",
            ]
        )
        XCTAssertEqual(ssh.remainingStepCount, 0)
        await ssh.shutdown()
    }

    func testCancellationIgnoringOldTokenFinishesBeforeReplacementTokenBegins() async throws {
        let events = LockedEventRecorder()
        let oldTokenGate = AsyncGate()
        let oldTokenData = try makeTokenData(
            sessionID: "session-original",
            localPort: 49_152,
            generation: 1
        )
        let ssh = ScriptedPreviewSSH(
            events: events,
            steps: [
                .response(
                    .status,
                    event: "status-original",
                    data: try makeStatusData(sessionID: "session-original")
                ),
                .operation(.token, event: "token-original-start") {
                    await withTaskCancellationHandler {
                        await oldTokenGate.wait()
                    } onCancel: {
                        events.append("token-original-cancelled")
                    }
                    events.append("token-original-finish")
                    return oldTokenData
                },
                .response(
                    .status,
                    event: "status-replacement",
                    data: try makeStatusData(sessionID: "session-replacement")
                ),
                .response(
                    .token,
                    event: "token-replacement-start",
                    data: try makeTokenData(
                        sessionID: "session-replacement",
                        localPort: 49_152,
                        generation: 2
                    )
                ),
            ]
        )
        let presenter = StatefulRecordingPreviewPresenter(events: events)
        let originalForward = NamedRecordingForward(
            name: "original",
            events: events,
            localPort: 49_152
        )
        let replacementForward = NamedRecordingForward(
            name: "replacement",
            events: events,
            localPort: 49_152
        )
        let coordinator = PreviewCoordinator(
            ssh: ssh,
            commands: try RemotePreviewCommandBuilder(workspacePath: "."),
            previewPresenter: presenter,
            forwardFactory: SequencedForwardFactory(
                forwards: [originalForward, replacementForward]
            )
        )

        let originalAttachment = Task {
            try await coordinator.attach(sessionID: "session-original")
        }
        await waitForEvent("token-original-start", in: events)
        originalAttachment.cancel()
        await waitForEvent("token-original-cancelled", in: events)

        let replacementAttachment = Task {
            try await coordinator.attach(sessionID: "session-replacement")
        }
        await waitForEvent("forward-close-original", in: events)
        for _ in 0 ..< 100 {
            await Task.yield()
        }
        XCTAssertFalse(events.snapshot().contains("status-replacement"))
        XCTAssertFalse(events.snapshot().contains("token-replacement-start"))

        oldTokenGate.open()
        _ = try? await originalAttachment.value
        try await replacementAttachment.value

        let snapshot = events.snapshot()
        let oldFinishIndex = try XCTUnwrap(
            snapshot.firstIndex(of: "token-original-finish")
        )
        let replacementStartIndex = try XCTUnwrap(
            snapshot.firstIndex(of: "token-replacement-start")
        )
        XCTAssertLessThan(oldFinishIndex, replacementStartIndex)
        XCTAssertEqual(
            coordinator.state,
            .attached(sessionID: "session-replacement", localPort: 49_152)
        )
        XCTAssertEqual(presenter.activeSessionID, "session-replacement")
        XCTAssertEqual(ssh.remainingStepCount, 0)

        await coordinator.detach()
        await ssh.shutdown()
    }
}

private enum ScriptedCommandKind: String, Sendable {
    case list
    case status
    case token

    init(command: String) throws {
        if command.contains("--issue-token") {
            self = .token
        } else if command.contains("status") {
            self = .status
        } else if command.contains("list") {
            self = .list
        } else {
            throw ScriptedPreviewError.unrecognizedCommand
        }
    }
}

private struct ScriptedSSHStep: Sendable {
    let kind: ScriptedCommandKind
    let event: String
    let body: @Sendable () async throws -> Data

    static func response(
        _ kind: ScriptedCommandKind,
        event: String,
        data: Data
    ) -> Self {
        operation(kind, event: event) { data }
    }

    static func failure(
        _ kind: ScriptedCommandKind,
        event: String,
        error: any Error & Sendable
    ) -> Self {
        operation(kind, event: event) { throw error }
    }

    static func operation(
        _ kind: ScriptedCommandKind,
        event: String,
        body: @escaping @Sendable () async throws -> Data
    ) -> Self {
        Self(kind: kind, event: event, body: body)
    }
}

private final class ScriptedPreviewSSH: PreviewSSHSession, @unchecked Sendable {
    let eventLoopGroup: EventLoopGroup
    private let group: MultiThreadedEventLoopGroup
    private let events: LockedEventRecorder
    private let lock = NSLock()
    private var steps: [ScriptedSSHStep]

    init(events: LockedEventRecorder, steps: [ScriptedSSHStep]) {
        self.events = events
        self.steps = steps
        group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        eventLoopGroup = group
    }

    var remainingStepCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return steps.count
    }

    func runCommand(
        _ command: String,
        maximumOutputBytes: Int
    ) async throws -> Data {
        let actualKind = try ScriptedCommandKind(command: command)
        guard let step = takeNextStep() else {
            throw ScriptedPreviewError.commandPlanExhausted
        }
        events.append(step.event)
        guard step.kind == actualKind else {
            throw ScriptedPreviewError.unexpectedCommand(
                expected: step.kind.rawValue,
                actual: actualKind.rawValue
            )
        }
        return try await step.body()
    }

    func connectDirectTCPIP(
        localChannel: Channel,
        destination: SSHForwardDestination
    ) -> EventLoopFuture<Void> {
        localChannel.eventLoop.makeSucceededFuture(())
    }

    func shutdown() async {
        try? await group.shutdownGracefully()
    }

    private func takeNextStep() -> ScriptedSSHStep? {
        lock.lock()
        defer { lock.unlock() }
        guard !steps.isEmpty else { return nil }
        return steps.removeFirst()
    }
}

@MainActor
private final class StatefulRecordingPreviewPresenter: PreviewAttachmentPresenting {
    private let events: LockedEventRecorder
    private(set) var activeSessionID: String?

    init(events: LockedEventRecorder) {
        self.events = events
    }

    func open(_ configuration: PreviewLaunchConfiguration) async throws {
        activeSessionID = configuration.session.sessionID
        events.append("open-\(configuration.session.sessionID)")
    }

    func setPresentation(_ presentation: PreviewPresentation) {}

    func close() {
        activeSessionID = nil
        events.append("presenter-close")
    }
}

@MainActor
private final class BlockingGenerationPreviewPresenter: PreviewAttachmentPresenting {
    private let events: LockedEventRecorder
    private let openGate: AsyncGate
    private var generation: UInt64 = 0
    private(set) var activeSessionID: String?

    init(events: LockedEventRecorder, openGate: AsyncGate) {
        self.events = events
        self.openGate = openGate
    }

    func open(_ configuration: PreviewLaunchConfiguration) async throws {
        let openGeneration = generation
        let sessionID = configuration.session.sessionID
        events.append("presenter-open-start-\(sessionID)")
        await openGate.wait()
        guard generation == openGeneration else {
            events.append("presenter-open-discarded-\(sessionID)")
            return
        }
        activeSessionID = sessionID
        events.append("presenter-open-committed-\(sessionID)")
    }

    func setPresentation(_ presentation: PreviewPresentation) {}

    func close() {
        generation &+= 1
        activeSessionID = nil
        events.append("presenter-close")
    }
}

private struct SequencedForwardFactory: PreviewLocalForwardCreating {
    private let queue: NamedForwardQueue

    init(forwards: [NamedRecordingForward]) {
        queue = NamedForwardQueue(forwards: forwards)
    }

    func makeForward(
        destination: SSHForwardDestination
    ) throws -> any PreviewLocalForwardListening {
        guard let forward = queue.takeNext() else {
            throw ScriptedPreviewError.forwardPlanExhausted
        }
        return forward
    }
}

private final class NamedForwardQueue: @unchecked Sendable {
    private let lock = NSLock()
    private var forwards: [NamedRecordingForward]

    init(forwards: [NamedRecordingForward]) {
        self.forwards = forwards
    }

    func takeNext() -> NamedRecordingForward? {
        lock.lock()
        defer { lock.unlock() }
        guard !forwards.isEmpty else { return nil }
        return forwards.removeFirst()
    }
}

private final class NamedRecordingForward: PreviewLocalForwardListening, @unchecked Sendable {
    private let name: String
    private let events: LockedEventRecorder
    private let localPort: UInt16
    private let lock = NSLock()
    private var isClosed = false
    private var closeCountStorage = 0

    init(name: String, events: LockedEventRecorder, localPort: UInt16) {
        self.name = name
        self.events = events
        self.localPort = localPort
    }

    var closeCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return closeCountStorage
    }

    func start() async throws -> UInt16 {
        events.append("listen-\(name)")
        return localPort
    }

    func close() async {
        guard markClosed() else { return }
        events.append("forward-close-\(name)")
    }

    private func markClosed() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !isClosed else { return false }
        isClosed = true
        closeCountStorage += 1
        return true
    }
}

private enum ScriptedPreviewError: Error, Sendable {
    case commandPlanExhausted
    case forwardPlanExhausted
    case unexpectedCommand(expected: String, actual: String)
    case unrecognizedCommand
}

private func makeStatusData(
    sessionID: String,
    remotePort: UInt16 = 7_331
) throws -> Data {
    try JSONSerialization.data(
        withJSONObject: [
            "protocolVersion": 1,
            "sessionId": sessionID,
            "state": "connected",
            "signaling": [
                "host": "127.0.0.1",
                "port": Int(remotePort),
                "path": "/signal",
            ],
            "health": [
                "heartbeatFresh": true,
                "tmuxAlive": true,
                "active": true,
            ],
        ],
        options: [.sortedKeys]
    )
}

private func makeTokenData(
    sessionID: String,
    localPort: UInt16,
    generation: Int
) throws -> Data {
    try JSONSerialization.data(
        withJSONObject: [
            "protocolVersion": 1,
            "sessionId": sessionID,
            "generation": generation,
            "nonce": "nonce_0123456789abcdef",
            "token": "token_0123456789abcdefghijklmnopqrstuvwxyz",
            "signaling": [
                "url": "ws://127.0.0.1:\(localPort)/signal",
            ],
            "profile": "mini",
            "provider": "webrtc",
            "presentation": "mini",
        ],
        options: [.sortedKeys]
    )
}

private func makeSessionListData(sessionID: String) throws -> Data {
    try JSONSerialization.data(
        withJSONObject: [
            "protocolVersion": 1,
            "sessions": [
                [
                    "sessionId": sessionID,
                    "state": "connected",
                    "signaling": [
                        "host": "127.0.0.1",
                        "port": 7_331,
                        "path": "/signal",
                    ],
                    "health": [
                        "heartbeatFresh": true,
                        "tmuxAlive": true,
                        "active": true,
                    ],
                ],
            ],
        ],
        options: [.sortedKeys]
    )
}

private final class RecordingPreviewSSH: PreviewSSHSession, @unchecked Sendable {
    let eventLoopGroup: EventLoopGroup
    private let group: MultiThreadedEventLoopGroup
    private let events: LockedEventRecorder
    private let tokenFailure: Error?
    private var firstTokenGate: AsyncGate?
    private let lock = NSLock()
    private(set) var commands: [String] = []

    init(
        events: LockedEventRecorder,
        tokenFailure: Error? = nil,
        firstTokenGate: AsyncGate? = nil
    ) {
        self.events = events
        self.tokenFailure = tokenFailure
        self.firstTokenGate = firstTokenGate
        group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        eventLoopGroup = group
    }

    func runCommand(
        _ command: String,
        maximumOutputBytes: Int
    ) async throws -> Data {
        record(command)

        if command.contains("status") && !command.contains("--issue-token") {
            events.append("status")
            return Data(
                """
                {
                  "protocolVersion": 1,
                  "sessionId": "session-019d",
                  "state": "connected",
                  "signaling": {
                    "host": "127.0.0.1",
                    "port": 7331,
                    "path": "/signal"
                  },
                  "health": {
                    "heartbeatFresh": true,
                    "tmuxAlive": true,
                    "active": true
                  }
                }
                """.utf8
            )
        }

        events.append("token")
        let gate = takeFirstTokenGate()
        if let gate {
            await withTaskCancellationHandler {
                await gate.wait()
            } onCancel: {
                gate.cancel()
            }
            try Task.checkCancellation()
        }
        if let tokenFailure { throw tokenFailure }
        let descriptor: [String: Any] = [
            "protocolVersion": 1,
            "sessionId": "session-019d",
            "generation": 3,
            "nonce": "nonce_0123456789abcdef",
            "token": "token_0123456789abcdefghijklmnopqrstuvwxyz",
            "signaling": ["url": "ws://127.0.0.1:49152/signal"],
            "profile": "expanded-headroom",
            "provider": "webrtc",
            "presentation": "expanded",
        ]
        return try JSONSerialization.data(
            withJSONObject: descriptor,
            options: [.sortedKeys]
        )
    }

    private func record(_ command: String) {
        lock.lock()
        defer { lock.unlock() }
        commands.append(command)
    }

    private func takeFirstTokenGate() -> AsyncGate? {
        lock.lock()
        defer { lock.unlock() }
        let gate = firstTokenGate
        firstTokenGate = nil
        return gate
    }

    func connectDirectTCPIP(
        localChannel: Channel,
        destination: SSHForwardDestination
    ) -> EventLoopFuture<Void> {
        localChannel.eventLoop.makeSucceededFuture(())
    }

    func shutdown() async {
        try? await group.shutdownGracefully()
    }
}

@MainActor
private final class RecordingPreviewPresenter: PreviewAttachmentPresenting {
    private let events: LockedEventRecorder

    init(events: LockedEventRecorder) {
        self.events = events
    }

    func open(_ configuration: PreviewLaunchConfiguration) async throws {
        events.append("open")
    }

    func setPresentation(_ presentation: PreviewPresentation) {}

    func close() {
        events.append("presenter-close")
    }
}

private struct RecordingForwardFactory: PreviewLocalForwardCreating {
    let events: LockedEventRecorder
    let localPort: UInt16

    func makeForward(
        destination: SSHForwardDestination
    ) throws -> any PreviewLocalForwardListening {
        RecordingForward(events: events, localPort: localPort)
    }
}

private struct FixedForwardFactory: PreviewLocalForwardCreating {
    let forward: any PreviewLocalForwardListening

    func makeForward(
        destination: SSHForwardDestination
    ) throws -> any PreviewLocalForwardListening {
        forward
    }
}

private final class RecordingForward: PreviewLocalForwardListening, @unchecked Sendable {
    private let events: LockedEventRecorder
    private let localPort: UInt16

    init(events: LockedEventRecorder, localPort: UInt16) {
        self.events = events
        self.localPort = localPort
    }

    func start() async throws -> UInt16 {
        events.append("listen")
        return localPort
    }

    func close() async {
        events.append("forward-close")
    }
}

private final class DelayedRecordingForward: PreviewLocalForwardListening, @unchecked Sendable {
    private let events: LockedEventRecorder
    private let localPort: UInt16
    private let startGate = AsyncGate()
    private let lock = NSLock()
    private var isClosed = false

    init(events: LockedEventRecorder, localPort: UInt16) {
        self.events = events
        self.localPort = localPort
    }

    func start() async throws -> UInt16 {
        events.append("listen")
        await startGate.wait()
        return localPort
    }

    func close() async {
        let shouldClose = markClosed()
        guard shouldClose else { return }
        events.append("forward-close")
        startGate.open()
    }

    private func markClosed() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        let shouldClose = !isClosed
        isClosed = true
        return shouldClose
    }
}

private final class AsyncGate: @unchecked Sendable {
    private let lock = NSLock()
    private var isOpen = false
    private var continuation: CheckedContinuation<Void, Never>?

    var wasCancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return wasCancelledStorage
    }

    private var wasCancelledStorage = false

    func wait() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if isOpen {
                lock.unlock()
                continuation.resume()
            } else {
                self.continuation = continuation
                lock.unlock()
            }
        }
    }

    func open() {
        lock.lock()
        isOpen = true
        let continuation = continuation
        self.continuation = nil
        lock.unlock()
        continuation?.resume()
    }

    func cancel() {
        lock.lock()
        wasCancelledStorage = true
        isOpen = true
        let continuation = continuation
        self.continuation = nil
        lock.unlock()
        continuation?.resume()
    }
}

private final class LockedEventRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var events: [String] = []

    func append(_ event: String) {
        lock.lock()
        events.append(event)
        lock.unlock()
    }

    func snapshot() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return events
    }
}

private enum TestFailure: Error {
    case expected
}

private func XCTAssertThrowsErrorAsync(
    _ expression: () async throws -> Void,
    file: StaticString = #filePath,
    line: UInt = #line
) async {
    do {
        try await expression()
        XCTFail("Expected async expression to throw", file: file, line: line)
    } catch {
        // Expected.
    }
}

@MainActor
private func waitForEvent(
    _ expected: String,
    in events: LockedEventRecorder,
    file: StaticString = #filePath,
    line: UInt = #line
) async {
    for _ in 0 ..< 2_000 {
        if events.snapshot().contains(expected) {
            return
        }
        await Task.yield()
    }
    XCTFail("Timed out waiting for event \(expected)", file: file, line: line)
}
