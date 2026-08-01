import Foundation
import NIOCore
import NIOPosix
import XCTest
@testable import PreviewHarness

final class CombinedWorkspaceTests: XCTestCase {
    func testPhysicalKeyboardLaunchGateRequiresExplicitLimitedModeAcknowledgement() {
        XCTAssertTrue(
            PhysicalKeyboardLaunchGate(
                availability: .present,
                softwareKeyboardAcknowledged: false
            ).permitsConnection
        )

        for availability in [
            PhysicalKeyboardAvailability.absent,
            PhysicalKeyboardAvailability.unknown,
        ] {
            XCTAssertFalse(
                PhysicalKeyboardLaunchGate(
                    availability: availability,
                    softwareKeyboardAcknowledged: false
                ).permitsConnection
            )
            XCTAssertTrue(
                PhysicalKeyboardLaunchGate(
                    availability: availability,
                    softwareKeyboardAcknowledged: true
                ).permitsConnection
            )
        }
    }

    func testRemoteProfileNormalizesWithoutContainingSecrets() throws {
        let id = UUID()
        let profile = try RemoteProfile(
            id: id,
            name: "  Remote host  ",
            host: "[FD7A:115C:A1E0::10]",
            port: 2_222,
            username: "developer",
            workspace: "~/projects/BIRDWORLD",
            previewToolsPath: "~/src/wscrpt",
            launchStyle: .tmux(session: "  birdworld_dev  "),
            authenticationMethod: .deviceKey
        )

        XCTAssertEqual(profile.id, id)
        XCTAssertEqual(profile.name, "Remote host")
        XCTAssertEqual(profile.host, "fd7a:115c:a1e0::10")
        XCTAssertEqual(profile.endpointDescription, "[fd7a:115c:a1e0::10]:2222")
        XCTAssertEqual(profile.connectionDescription, "developer@[fd7a:115c:a1e0::10]:2222")
        XCTAssertEqual(profile.previewToolsPath, "~/src/wscrpt")
        XCTAssertEqual(profile.launchStyle, .tmux(session: "birdworld_dev"))

        let encoded = try JSONEncoder().encode(profile)
        let json = try XCTUnwrap(String(data: encoded, encoding: .utf8))
        XCTAssertFalse(json.localizedCaseInsensitiveContains("password"))
        XCTAssertFalse(json.localizedCaseInsensitiveContains("privateKey"))
    }

    func testSavedPasswordAccountIsBoundToExactEndpointIdentity() throws {
        let id = UUID()
        let original = try RemoteProfile(
            id: id,
            name: "Original",
            host: "remotehost.local",
            port: 22,
            username: "developer"
        )
        let sameEndpoint = try RemoteProfile(
            id: id,
            name: "Renamed",
            host: "REMOTEHOST.LOCAL",
            port: 22,
            username: "developer",
            workspace: "/another/workspace"
        )
        let editedHost = try RemoteProfile(
            id: id,
            name: "Other",
            host: "other.local",
            port: 22,
            username: "developer"
        )
        let editedUser = try RemoteProfile(
            id: id,
            name: "Other user",
            host: "remotehost.local",
            port: 22,
            username: "root"
        )

        XCTAssertEqual(
            SSHSecretAccount.password(profile: original),
            SSHSecretAccount.password(profile: sameEndpoint)
        )
        XCTAssertNotEqual(
            SSHSecretAccount.password(profile: original),
            SSHSecretAccount.password(profile: editedHost)
        )
        XCTAssertNotEqual(
            SSHSecretAccount.password(profile: original),
            SSHSecretAccount.password(profile: editedUser)
        )
    }

    func testTerminalOutboundBufferIsBoundedAndCoalescesResize() throws {
        var buffer = TerminalOutboundBuffer()
        let firstSize = try SSHTerminalSize(columns: 80, rows: 24)
        let latestSize = try SSHTerminalSize(columns: 120, rows: 40)
        buffer.replacePendingResize(with: firstSize)
        buffer.replacePendingResize(with: latestSize)
        try buffer.appendInput(Data(repeating: 0x61, count: 300_000))

        XCTAssertEqual(buffer.takeNext(), .resize(latestSize))
        guard case let .input(firstChunk) = buffer.takeNext() else {
            return XCTFail("Expected a terminal input chunk")
        }
        XCTAssertEqual(firstChunk.count, SSHTransport.maximumInputChunkBytes)
        XCTAssertEqual(buffer.pendingInputByteCount, 300_000 - firstChunk.count)

        XCTAssertThrowsError(
            try buffer.appendInput(
                Data(
                    repeating: 0x62,
                    count: TerminalOutboundBuffer.maximumPendingInputBytes
                )
            )
        )
    }

    func testDraftRejectsAmbiguousHostAndInvalidPort() {
        var draft = validDraft()
        draft.host = "remotehost.local:2222"
        XCTAssertThrowsError(try draft.validatedProfile()) { error in
            XCTAssertEqual(error as? RemoteProfileValidationError, .invalidHost)
        }

        for invalidPort in ["0", "65536", "22.0", "ssh"] {
            draft = validDraft()
            draft.port = invalidPort
            XCTAssertThrowsError(try draft.validatedProfile()) { error in
                XCTAssertEqual(error as? RemoteProfileValidationError, .invalidPort)
            }
        }
    }

    func testProfileRejectsControlCharactersAndUnsafeTmuxName() {
        var draft = validDraft()
        draft.workspace = "/srv/game\nother"
        XCTAssertThrowsError(try draft.validatedProfile()) { error in
            XCTAssertEqual(error as? RemoteProfileValidationError, .invalidWorkspace)
        }

        draft = validDraft()
        draft.previewToolsPath = "/srv/wscrpt\nother"
        XCTAssertThrowsError(try draft.validatedProfile()) { error in
            XCTAssertEqual(
                error as? RemoteProfileValidationError,
                .invalidPreviewToolsPath
            )
        }

        draft = validDraft()
        draft.tmuxSession = "wscrpt:other"
        XCTAssertThrowsError(try draft.validatedProfile()) { error in
            XCTAssertEqual(error as? RemoteProfileValidationError, .invalidTmuxSession)
        }
    }

    func testDecodedProfilesPassTheSameValidationGate() throws {
        let profile = try validDraft().validatedProfile()
        let canonicalData = try JSONEncoder().encode(profile)
        var object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: canonicalData) as? [String: Any]
        )
        object["host"] = "attacker.example:22"
        let damagedData = try JSONSerialization.data(withJSONObject: object)

        XCTAssertThrowsError(try JSONDecoder().decode(RemoteProfile.self, from: damagedData)) { error in
            XCTAssertEqual(error as? RemoteProfileValidationError, .invalidHost)
        }

        object["host"] = profile.host
        object.removeValue(forKey: "previewToolsPath")
        let legacyData = try JSONSerialization.data(withJSONObject: object)
        let migrated = try JSONDecoder().decode(RemoteProfile.self, from: legacyData)
        XCTAssertEqual(migrated.previewToolsPath, ".")
    }

    func testDirectLaunchCommandQuotesWorkspaceAsOneArgument() throws {
        let profile = try RemoteProfile(
            name: "Game",
            host: "remotehost.local",
            username: "developer",
            workspace: "/srv/Developer's Game",
            launchStyle: .direct
        )

        XCTAssertEqual(
            RemoteLaunchCommandBuilder.command(for: profile),
            "cd -- '/srv/Developer'\"'\"'s Game' && exec wscrpt ."
        )
    }

    func testDirectLaunchTreatsDashWorkspaceAsAPathNotCdOptions() throws {
        let profile = try RemoteProfile(
            name: "Dash path",
            host: "remotehost.local",
            username: "developer",
            workspace: "-P",
            launchStyle: .direct
        )

        XCTAssertEqual(
            RemoteLaunchCommandBuilder.command(for: profile),
            "cd -- '-P' && exec wscrpt ."
        )
    }

    func testTmuxLaunchAttachesOrCreatesAndExpandsHomeSafely() throws {
        let profile = try RemoteProfile(
            name: "Birdworld",
            host: "remotehost.local",
            username: "developer",
            workspace: "~/projects/BIRDWORLD",
            launchStyle: .tmux(session: "birdworld_dev")
        )

        XCTAssertEqual(
            RemoteLaunchCommandBuilder.command(for: profile),
            "exec tmux new-session -A -s 'birdworld_dev' -c \"$HOME\"/'projects/BIRDWORLD' 'exec wscrpt .'"
        )
    }

    func testShellQuoteDoesNotInterpolateMetacharacters() {
        let value = "$(touch /tmp/pwn); `id`; it's literal"
        let quoted = RemoteLaunchCommandBuilder.shellQuote(value)

        XCTAssertEqual(
            quoted,
            "'$(touch /tmp/pwn); `id`; it'\"'\"'s literal'"
        )
        XCTAssertFalse(quoted.contains("\n"))
    }

    @MainActor
    func testNewConnectionWaitsForPriorResourceRetirement() async {
        let gate = WorkspaceResourceRetirementGate()
        let events = RetirementEventRecorder()
        var releaseContinuation: AsyncStream<Void>.Continuation?
        let release = AsyncStream<Void> { continuation in
            releaseContinuation = continuation
        }

        let retirement = Task { @MainActor in
            await gate.enqueueAndWait {
                events.append("old-retirement-started")
                for await _ in release {
                    break
                }
                events.append("old-retirement-finished")
            }
        }
        while !events.values.contains("old-retirement-started") {
            await Task.yield()
        }

        let replacement = Task { @MainActor in
            events.append("replacement-waiting")
            await gate.waitForIdle()
            events.append("replacement-opened")
        }
        while !events.values.contains("replacement-waiting") {
            await Task.yield()
        }

        XCTAssertFalse(events.values.contains("replacement-opened"))
        releaseContinuation?.yield(())
        releaseContinuation?.finish()
        await retirement.value
        await replacement.value

        XCTAssertEqual(
            events.values,
            [
                "old-retirement-started",
                "replacement-waiting",
                "old-retirement-finished",
                "replacement-opened",
            ]
        )
    }

    func testLocalForwardReclaimsCapacityAfterRejectingSeventeenthClient() async throws {
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        let recorder = ForwardConnectorRecorder()
        let destination = try SSHForwardDestination(
            host: "127.0.0.1",
            port: 7_331
        )
        let forward = SSHLocalForward(
            eventLoopGroup: group,
            destination: destination
        ) { localChannel, _ in
            recorder.record(localChannel)
            return localChannel.pipeline.addHandler(
                CloseOnRemoteInputClosedHandler()
            )
        }
        var clients: [Channel] = []
        var operationError: Error?

        do {
            let localPort = try await forward.start()
            for _ in 0 ..< SSHLocalForward.maximumAcceptedConnections {
                clients.append(
                    try await connectLoopbackClient(
                        group: group,
                        port: localPort
                    )
                )
            }

            let allCapacityClientsReachedConnector = await waitUntil {
                recorder.count == 16
            }
            XCTAssertTrue(
                allCapacityClientsReachedConnector,
                "Expected all sixteen accepted clients to reach the connector"
            )
            XCTAssertEqual(recorder.count, 16)

            do {
                let excessClient = try await connectLoopbackClient(
                    group: group,
                    port: localPort
                )
                clients.append(excessClient)
                let excessClientClosed = await waitUntil {
                    !excessClient.isActive
                }
                XCTAssertTrue(
                    excessClientClosed,
                    "The seventeenth client stayed active instead of being rejected"
                )
            } catch {
                // Rejection may race the client-side connect completion. A
                // thrown connect and an immediately closed channel are both
                // valid observable outcomes for the excess socket.
            }
            XCTAssertEqual(
                recorder.count,
                16,
                "The rejected client must never reach the SSH connector"
            )

            let releasedClient = clients.removeFirst()
            try await releasedClient.close()
            if let releasedServerChannel = recorder.channel(at: 0) {
                let releasedServerChannelClosed = await waitUntil {
                    !releasedServerChannel.isActive
                }
                XCTAssertTrue(
                    releasedServerChannelClosed,
                    "Closing a client did not retire its accepted server channel"
                )
            } else {
                XCTFail("Expected the connector to record the first accepted channel")
            }

            let replacementClient = try await connectLoopbackClient(
                group: group,
                port: localPort
            )
            clients.append(replacementClient)
            let replacementReachedConnector = await waitUntil {
                recorder.count == 17
            }
            XCTAssertTrue(
                replacementReachedConnector,
                "A released capacity slot was not reclaimed"
            )
            XCTAssertEqual(recorder.count, 17)
            XCTAssertTrue(replacementClient.isActive)
        } catch {
            operationError = error
        }

        for client in clients {
            try? await client.close()
        }
        await forward.close()
        try? await group.shutdownGracefully()

        if let operationError {
            throw operationError
        }
    }

    private func validDraft() -> RemoteProfileDraft {
        RemoteProfileDraft(
            name: "Remote host",
            host: "remotehost.local",
            port: "22",
            username: "developer",
            workspace: "/srv/wscrpt",
            usesTmux: true,
            tmuxSession: "wscrpt",
            authenticationMethod: .password
        )
    }
}

@MainActor
private final class RetirementEventRecorder {
    private(set) var values: [String] = []

    func append(_ value: String) {
        values.append(value)
    }
}

private final class ForwardConnectorRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var channels: [Channel] = []

    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return channels.count
    }

    func record(_ channel: Channel) {
        lock.lock()
        channels.append(channel)
        lock.unlock()
    }

    func channel(at index: Int) -> Channel? {
        lock.lock()
        defer { lock.unlock() }
        guard channels.indices.contains(index) else { return nil }
        return channels[index]
    }
}

private final class CloseOnRemoteInputClosedHandler: ChannelInboundHandler,
    @unchecked Sendable
{
    typealias InboundIn = ByteBuffer

    func channelActive(context: ChannelHandlerContext) {
        context.read()
        context.fireChannelActive()
    }

    func channelReadComplete(context: ChannelHandlerContext) {
        context.read()
        context.fireChannelReadComplete()
    }

    func userInboundEventTriggered(
        context: ChannelHandlerContext,
        event: Any
    ) {
        if let channelEvent = event as? ChannelEvent,
           case .inputClosed = channelEvent {
            context.close(promise: nil)
            return
        }
        context.fireUserInboundEventTriggered(event)
    }
}

private func connectLoopbackClient(
    group: EventLoopGroup,
    port: UInt16
) async throws -> Channel {
    try await ClientBootstrap(group: group)
        .channelOption(
            ChannelOptions.connectTimeout,
            value: .seconds(2)
        )
        .connect(host: "127.0.0.1", port: Int(port))
        .get()
}

private func waitUntil(
    timeoutNanoseconds: UInt64 = 2_000_000_000,
    condition: @escaping @Sendable () -> Bool
) async -> Bool {
    let start = DispatchTime.now().uptimeNanoseconds
    while DispatchTime.now().uptimeNanoseconds - start < timeoutNanoseconds {
        if condition() {
            return true
        }
        try? await Task.sleep(nanoseconds: 1_000_000)
    }
    return condition()
}
