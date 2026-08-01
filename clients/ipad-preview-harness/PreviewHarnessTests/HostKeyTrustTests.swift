import CryptoKit
import Foundation
import NIOCore
import NIOPosix
import NIOSSH
import XCTest
@testable import PreviewHarness

final class HostKeyTrustTests: XCTestCase {
    private static let fixturePublicKey =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHbX0QDfR4YtDeSWldWtGXtrMiIyRO1jPOeKvK5OPu+1 fixture"
    private static let changedFixturePublicKey =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIO6L3v/AazNDmj20OFMBuSs0/7GzLm5TIRI3rJU8+/p3 fixture2"

    func testFingerprintMatchesOpenSSHKeygenSHA256Format() throws {
        let key = try NIOSSHPublicKey(openSSHPublicKey: Self.fixturePublicKey)
        let fingerprint = try SSHHostKeyFingerprint(hostKey: key)

        XCTAssertEqual(fingerprint.algorithm, "ssh-ed25519")
        XCTAssertEqual(
            fingerprint.description,
            "SHA256:toT/awUR63zw0mLW1Q+I6hmyBIDPVdcV7ITn0AgebM0"
        )
    }

    func testEndpointNormalizesDNSCaseAndIncludesPortInPinIdentity() throws {
        let endpoint = try SSHHostEndpoint(host: " Remotehost.Local ", port: 2_222)

        XCTAssertEqual(endpoint.host, "remotehost.local")
        XCTAssertEqual(endpoint.pinKey, "remotehost.local:2222")
        XCTAssertThrowsError(try SSHHostEndpoint(host: "bad host", port: 22))
        XCTAssertThrowsError(try SSHHostEndpoint(host: "remotehost", port: 0))
    }

    func testTOFURequiresConfirmationThenRejectsChangedKey() throws {
        let first = try SSHHostKeyFingerprint(
            hostKey: NIOSSHPublicKey(
                openSSHPublicKey: Self.fixturePublicKey
            )
        )
        let changed = try SSHHostKeyFingerprint(
            hostKey: NIOSSHPublicKey(
                openSSHPublicKey: Self.changedFixturePublicKey
            )
        )

        XCTAssertEqual(
            SSHHostKeyPinPolicy.evaluate(
                mode: .trustOnFirstUse,
                stored: nil,
                presented: first
            ),
            .requireFirstUseConfirmation
        )
        XCTAssertEqual(
            SSHHostKeyPinPolicy.evaluate(
                mode: .trustOnFirstUse,
                stored: first,
                presented: first
            ),
            .acceptKnownKey
        )
        XCTAssertEqual(
            SSHHostKeyPinPolicy.evaluate(
                mode: .trustOnFirstUse,
                stored: first,
                presented: changed
            ),
            .rejectChangedKey(expected: first, presented: changed)
        )
    }

    func testPinIfAbsentNeverOverwritesConcurrentAuthoritativePin() throws {
        let suiteName = "HostKeyTrustTests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let store = UserDefaultsSSHHostKeyPinStore(
            defaults: defaults,
            namespace: "test.pin"
        )
        let endpoint = try SSHHostEndpoint(host: "127.0.0.1", port: 22)
        let first = try SSHHostKeyFingerprint(
            hostKey: NIOSSHPublicKey(
                openSSHPublicKey: Self.fixturePublicKey
            )
        )
        let changed = try SSHHostKeyFingerprint(
            hostKey: NIOSSHPublicKey(
                openSSHPublicKey: Self.changedFixturePublicKey
            )
        )

        XCTAssertEqual(
            try store.pinIfAbsent(first, for: endpoint),
            first
        )
        XCTAssertEqual(
            try store.pinIfAbsent(changed, for: endpoint),
            first
        )
        XCTAssertEqual(
            try store.pinnedFingerprint(for: endpoint),
            first
        )
    }

    func testExplicitPinDoesNotConsultDamagedTOFUStore() throws {
        let key = try NIOSSHPublicKey(openSSHPublicKey: Self.fixturePublicKey)
        let fingerprint = try SSHHostKeyFingerprint(hostKey: key)
        let endpoint = try SSHHostEndpoint(host: "remotehost.local", port: 22)
        let store = FailingHostKeyPinStore()
        let delegate = try StrictSSHHostKeyTrustDelegate(
            endpoint: endpoint,
            mode: .pinned(fingerprint),
            pinStore: store
        )
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        defer { XCTAssertNoThrow(try group.syncShutdownGracefully()) }
        let promise = group.next().makePromise(of: Void.self)

        delegate.validateHostKey(
            hostKey: key,
            validationCompletePromise: promise
        )

        XCTAssertNoThrow(try promise.futureResult.wait())
        XCTAssertEqual(store.readCount, 0)
    }

    func testGeneratedIdentityRestoresSameAuthorizedKey() throws {
        let generated = SSHEd25519Identity()
        let restored = try SSHEd25519Identity(
            rawPrivateKeyRepresentation: generated.rawPrivateKeyRepresentation
        )

        XCTAssertEqual(
            restored.rawPrivateKeyRepresentation,
            generated.rawPrivateKeyRepresentation
        )
        XCTAssertEqual(restored.openSSHPublicKey, generated.openSSHPublicKey)
        XCTAssertThrowsError(
            try SSHEd25519Identity(rawPrivateKeyRepresentation: Data([0]))
        )
    }

    func testConnectionCannotReuseTrustDelegateForDifferentEndpoint() throws {
        let trusted = try SSHHostEndpoint(host: "remotehost.local", port: 22)
        let other = try SSHHostEndpoint(host: "other.local", port: 22)
        let trust = try StrictSSHHostKeyTrustDelegate(
            endpoint: trusted,
            mode: .trustOnFirstUse,
            pinStore: UserDefaultsSSHHostKeyPinStore(
                namespace: "test.endpoint-mismatch.\(UUID().uuidString)"
            )
        )

        XCTAssertThrowsError(
            try SSHConnectionConfiguration(
                endpoint: other,
                username: "developer",
                credentials: [.password("secret")],
                hostKeyTrust: trust
            )
        ) { error in
            XCTAssertEqual(
                error as? SSHTransportError,
                .invalidConfiguration
            )
        }
    }

    func testTerminalAndExecBoundsFailClosed() throws {
        XCTAssertNoThrow(
            try SSHTerminalSize(columns: 80, rows: 24)
        )
        XCTAssertThrowsError(
            try SSHTerminalSize(columns: 0, rows: 24)
        )
        XCTAssertNoThrow(
            try SSHExecLimits(
                maximumOutputBytes: 1_048_576,
                timeoutSeconds: 30
            )
        )
        XCTAssertThrowsError(
            try SSHExecLimits(
                maximumOutputBytes: 16_777_217,
                timeoutSeconds: 30
            )
        )
    }
}

final class SSHTransportIntegrationTests: XCTestCase {
    func testPinnedEd25519PTYExecAndDirectForward() async throws {
        let identity = SSHEd25519Identity()
        let expectedClientKey = try NIOSSHPublicKey(
            openSSHPublicKey: identity.openSSHPublicKey
        )
        let serverState = HermeticSSHServerState()
        let server = HermeticNIOSSHServer(
            expectedUsername: "integration",
            expectedClientKey: expectedClientKey,
            state: serverState
        )
        let serverPort = try await server.start()

        let output = HermeticOutputRecorder()
        let transport = SSHTransport(
            callbackQueue: .main,
            callbacks: .init(
                onOutput: { output.append($0.data) }
            )
        )
        addTeardownBlock {
            await transport.close()
            await server.stop()
        }

        let endpoint = try SSHHostEndpoint(
            host: "127.0.0.1",
            port: serverPort
        )
        let exactFingerprint = try SSHHostKeyFingerprint(
            hostKey: server.hostPublicKey
        )
        let trust = try StrictSSHHostKeyTrustDelegate(
            endpoint: endpoint,
            mode: .pinned(exactFingerprint)
        )
        let configuration = try SSHConnectionConfiguration(
            endpoint: endpoint,
            username: "integration",
            credentials: [.ed25519(identity)],
            hostKeyTrust: trust,
            connectTimeoutSeconds: 5
        )
        let initialSize = try SSHTerminalSize(
            columns: 80,
            rows: 24,
            pixelWidth: 1_024,
            pixelHeight: 768
        )

        try await transport.connect(
            configuration: configuration,
            initialSize: initialSize
        )
        XCTAssertEqual(transport.state, .connected)
        try await output.waitUntilContains("shell-ready")

        try await transport.send(Data("hello-pty".utf8))
        try await output.waitUntilContains("shell:hello-pty")

        let resized = try SSHTerminalSize(
            columns: 101,
            rows: 37,
            pixelWidth: 1_414,
            pixelHeight: 922
        )
        try await transport.resize(resized)
        try await output.waitUntilContains("resize:101x37")

        let execResult = try await transport.execute(
            "integration-ok",
            limits: try SSHExecLimits(
                maximumOutputBytes: 128,
                timeoutSeconds: 5
            )
        )
        XCTAssertEqual(execResult.exitStatus, 0)
        XCTAssertEqual(
            String(data: execResult.standardOutput, encoding: .utf8),
            "exec-stdout"
        )
        XCTAssertEqual(
            String(data: execResult.standardError, encoding: .utf8),
            "exec-stderr"
        )

        do {
            _ = try await transport.execute(
                "integration-flood",
                limits: try SSHExecLimits(
                    maximumOutputBytes: 8,
                    timeoutSeconds: 5
                )
            )
            XCTFail("Expected the bounded exec output limit to close the child")
        } catch {
            XCTAssertEqual(
                error as? SSHTransportError,
                .execOutputLimitExceeded
            )
        }

        let destination = try SSHForwardDestination(
            host: "127.0.0.1",
            port: HermeticNIOSSHServer.echoDestinationPort
        )
        let forward = SSHLocalForward(
            eventLoopGroup: transport.eventLoopGroup,
            destination: destination,
            connector: transport.makeDirectTCPIPConnector()
        )
        addTeardownBlock {
            await forward.close()
        }
        let localPort = try await forward.start()

        let tunnelPayload = Data("direct-tcpip-loopback".utf8)
        let responseLoop = transport.eventLoopGroup.next()
        let responsePromise = responseLoop.makePromise(of: Data.self)
        let responseTimeout = responseLoop.scheduleTask(in: .seconds(5)) {
            responsePromise.fail(HermeticSSHTestError.timedOut)
        }
        defer { responseTimeout.cancel() }

        let tunnelClient = try await ClientBootstrap(
            group: transport.eventLoopGroup
        )
        .channelOption(
            ChannelOptions.socketOption(.tcp_nodelay),
            value: 1
        )
        .channelInitializer { channel in
            channel.pipeline.addHandler(
                HermeticTCPResponseHandler(
                    expectedByteCount: tunnelPayload.count,
                    responsePromise: responsePromise
                )
            )
        }
        .connect(host: "127.0.0.1", port: Int(localPort))
        .get()
        addTeardownBlock {
            try? await tunnelClient.close()
        }

        var tunnelBuffer = tunnelClient.allocator.buffer(
            capacity: tunnelPayload.count
        )
        tunnelBuffer.writeBytes(tunnelPayload)
        try await tunnelClient.writeAndFlush(tunnelBuffer).get()
        let tunneledResponse = try await responsePromise.futureResult.get()
        XCTAssertEqual(tunneledResponse, tunnelPayload)

        // The transport, not only the loopback listener, owns every live
        // direct-tcpip child. Closing it must tear down the bridged local
        // socket even while the listener itself remains open.
        await transport.close()
        try await waitForFuture(
            tunnelClient.closeFuture,
            timeoutSeconds: 2
        )
        await forward.close()

        XCTAssertTrue(serverState.didAuthenticateExpectedKey)
        XCTAssertEqual(
            serverState.initialPTY,
            HermeticTerminalSnapshot(
                columns: 80,
                rows: 24,
                pixelWidth: 1_024,
                pixelHeight: 768
            )
        )
        XCTAssertEqual(
            serverState.latestResize,
            HermeticTerminalSnapshot(
                columns: 101,
                rows: 37,
                pixelWidth: 1_414,
                pixelHeight: 922
            )
        )
        XCTAssertEqual(serverState.shellInput, Data("hello-pty".utf8))
        XCTAssertEqual(serverState.directTargetHost, "127.0.0.1")
        XCTAssertEqual(
            serverState.directTargetPort,
            HermeticNIOSSHServer.echoDestinationPort
        )

        XCTAssertEqual(transport.state, .closed)
    }

    func testPinnedPasswordAuthenticationOpensPTY() async throws {
        let password = "correct horse battery staple"
        let serverState = HermeticSSHServerState()
        let server = HermeticNIOSSHServer(
            expectedUsername: "integration",
            expectedPassword: password,
            state: serverState
        )
        let serverPort = try await server.start()
        let output = HermeticOutputRecorder()
        let transport = SSHTransport(
            callbacks: .init(onOutput: { output.append($0.data) })
        )
        addTeardownBlock {
            await transport.close()
            await server.stop()
        }

        let endpoint = try SSHHostEndpoint(
            host: "127.0.0.1",
            port: serverPort
        )
        let trust = try StrictSSHHostKeyTrustDelegate(
            endpoint: endpoint,
            mode: .pinned(
                try SSHHostKeyFingerprint(hostKey: server.hostPublicKey)
            )
        )
        try await transport.connect(
            configuration: try SSHConnectionConfiguration(
                endpoint: endpoint,
                username: "integration",
                credentials: [.password(password)],
                hostKeyTrust: trust,
                connectTimeoutSeconds: 5,
                handshakeTimeoutSeconds: 5
            ),
            initialSize: try SSHTerminalSize(columns: 80, rows: 24)
        )

        XCTAssertEqual(transport.state, .connected)
        XCTAssertTrue(serverState.didAuthenticateExpectedPassword)
        try await output.waitUntilContains("shell-ready")
    }

    func testRejectedPasswordNeverOpensPTY() async throws {
        let serverState = HermeticSSHServerState()
        let server = HermeticNIOSSHServer(
            expectedUsername: "integration",
            expectedPassword: "correct password",
            state: serverState
        )
        let serverPort = try await server.start()
        let transport = SSHTransport()
        addTeardownBlock {
            await transport.close()
            await server.stop()
        }

        let endpoint = try SSHHostEndpoint(
            host: "127.0.0.1",
            port: serverPort
        )
        let trust = try StrictSSHHostKeyTrustDelegate(
            endpoint: endpoint,
            mode: .pinned(
                try SSHHostKeyFingerprint(hostKey: server.hostPublicKey)
            )
        )
        do {
            try await transport.connect(
                configuration: try SSHConnectionConfiguration(
                    endpoint: endpoint,
                    username: "integration",
                    credentials: [.password("wrong password")],
                    hostKeyTrust: trust,
                    connectTimeoutSeconds: 5,
                    handshakeTimeoutSeconds: 1
                ),
                initialSize: try SSHTerminalSize(columns: 80, rows: 24)
            )
            XCTFail("A rejected password must not open the terminal")
        } catch {
            // The exact NIOSSH authentication error is intentionally opaque;
            // the fail-closed transport state and absence of a PTY are the
            // product-level contract.
        }

        guard case .failed = transport.state else {
            XCTFail("Rejected password must leave the transport failed")
            return
        }
        XCTAssertFalse(serverState.didAuthenticateExpectedPassword)
        XCTAssertNil(serverState.initialPTY)
    }

    func testPTYReadinessUsesOverallHandshakeDeadline() async throws {
        let fixture = try await makeFixture(serverBehavior: .stallPTYReply)
        let transport = SSHTransport()
        addTeardownBlock {
            await transport.close()
            await fixture.server.stop()
        }

        let configuration = try SSHConnectionConfiguration(
            endpoint: fixture.endpoint,
            username: "integration",
            credentials: [.ed25519(fixture.identity)],
            hostKeyTrust: fixture.trust,
            connectTimeoutSeconds: 5,
            handshakeTimeoutSeconds: 1
        )

        let start = ContinuousClock.now
        do {
            try await transport.connect(
                configuration: configuration,
                initialSize: try SSHTerminalSize(columns: 80, rows: 24)
            )
            XCTFail("A server that never answers PTY setup must time out")
        } catch {
            XCTAssertEqual(error as? SSHTransportError, .connectionTimedOut)
        }
        XCTAssertLessThan(start.duration(to: .now), .seconds(4))
    }

    func testExecDeadlineIncludesStalledChildCreation() async throws {
        let fixture = try await makeFixture(
            serverBehavior: .stallSecondSessionInitializer
        )
        let transport = SSHTransport()
        addTeardownBlock {
            await transport.close()
            await fixture.server.stop()
        }
        try await connect(transport, fixture: fixture)

        let start = ContinuousClock.now
        do {
            _ = try await transport.execute(
                "integration-ok",
                limits: try SSHExecLimits(
                    maximumOutputBytes: 128,
                    timeoutSeconds: 1
                )
            )
            XCTFail("A stalled exec child open must time out")
        } catch {
            XCTAssertEqual(error as? SSHTransportError, .execTimedOut)
        }
        XCTAssertLessThan(start.duration(to: .now), .seconds(4))
    }

    func testCancelledLateOpenExecReturnsPromptlyWithoutSendingRequest() async throws {
        let fixture = try await makeFixture(
            serverBehavior: .holdSecondSessionInitializer
        )
        let transport = SSHTransport()
        addTeardownBlock {
            await transport.close()
            await fixture.server.stop()
        }
        try await connect(transport, fixture: fixture)

        let cancelledCommand = "must-never-execute"
        let execTask = Task {
            try await transport.execute(
                cancelledCommand,
                limits: try SSHExecLimits(
                    maximumOutputBytes: 128,
                    timeoutSeconds: 10
                )
            )
        }
        try await waitUntil {
            fixture.state.isHoldingSecondSessionInitializer
        }

        let cancellationStart = ContinuousClock.now
        execTask.cancel()
        do {
            _ = try await execTask.value
            XCTFail("A cancelled pending exec must not succeed")
        } catch {
            XCTAssertTrue(error is CancellationError)
        }
        XCTAssertLessThan(
            cancellationStart.duration(to: .now),
            .seconds(2)
        )
        XCTAssertTrue(fixture.state.execCommands.isEmpty)

        // Completing the server-side child initializer after cancellation is
        // the dangerous late-open path. The client's cancellation gate must
        // close that child before channelActive can emit ExecRequest.
        fixture.server.releaseHeldSessionInitializer()
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertFalse(
            fixture.state.execCommands.contains(cancelledCommand)
        )

        let successorCommand = "integration-ok"
        let successor = try await transport.execute(
            successorCommand,
            limits: try SSHExecLimits(
                maximumOutputBytes: 128,
                timeoutSeconds: 5
            )
        )
        XCTAssertEqual(successor.exitStatus, 0)
        XCTAssertEqual(fixture.state.execCommands, [successorCommand])
        XCTAssertEqual(transport.state, .connected)
    }

    func testCancelledLateExecCannotRegisterInReplacementGeneration() async throws {
        let fixture = try await makeFixture(
            serverBehavior: .holdSecondSessionInitializer
        )
        let transport = SSHTransport()
        addTeardownBlock {
            await transport.close()
            await fixture.server.stop()
        }
        try await connect(transport, fixture: fixture)

        let staleCommand = "stale-generation-command"
        let staleExec = Task {
            try await transport.execute(
                staleCommand,
                limits: try SSHExecLimits(
                    maximumOutputBytes: 128,
                    timeoutSeconds: 10
                )
            )
        }
        try await waitUntil {
            fixture.state.isHoldingSecondSessionInitializer
        }

        staleExec.cancel()
        await transport.close()
        do {
            _ = try await staleExec.value
            XCTFail("The stale generation's cancelled exec must not succeed")
        } catch {
            XCTAssertTrue(error is CancellationError)
        }

        // Reconnect the same transport before allowing the old server-side
        // initializer to complete. A late child still carries the prior
        // generation and must never become owned by this replacement session.
        try await connect(transport, fixture: fixture)
        XCTAssertEqual(transport.state, .connected)
        fixture.server.releaseHeldSessionInitializer()
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertFalse(fixture.state.execCommands.contains(staleCommand))

        let replacementCommand = "integration-ok"
        let replacement = try await transport.execute(
            replacementCommand,
            limits: try SSHExecLimits(
                maximumOutputBytes: 128,
                timeoutSeconds: 5
            )
        )
        XCTAssertEqual(replacement.exitStatus, 0)
        XCTAssertEqual(fixture.state.execCommands, [replacementCommand])
        XCTAssertEqual(transport.state, .connected)
    }

    func testDirectOpenDeadlineClosesRootAndOriginator() async throws {
        let fixture = try await makeFixture(serverBehavior: .normal)
        let transport = SSHTransport(
            directOpenTimeout: .milliseconds(250)
        )
        addTeardownBlock {
            await transport.close()
            await fixture.server.stop()
        }
        try await connect(transport, fixture: fixture)
        XCTAssertTrue(fixture.server.pauseEventLoop())
        defer { fixture.server.resumeEventLoop() }

        let forward = SSHLocalForward(
            eventLoopGroup: transport.eventLoopGroup,
            destination: try SSHForwardDestination(
                host: "127.0.0.1",
                port: HermeticNIOSSHServer.echoDestinationPort
            ),
            connector: transport.makeDirectTCPIPConnector()
        )
        addTeardownBlock { await forward.close() }
        let localPort = try await forward.start()
        let client = try await ClientBootstrap(group: transport.eventLoopGroup)
            .connect(host: "127.0.0.1", port: Int(localPort))
            .get()
        addTeardownBlock { try? await client.close() }

        let start = ContinuousClock.now
        try await waitForFuture(client.closeFuture, timeoutSeconds: 3)
        XCTAssertLessThan(start.duration(to: .now), .seconds(2))
        try await waitUntilTransportFails(transport)
    }

    func testClosingOriginatorDuringPendingDirectOpenFailsClosed() async throws {
        let fixture = try await makeFixture(serverBehavior: .normal)
        let transport = SSHTransport(
            directOpenTimeout: .seconds(5)
        )
        addTeardownBlock {
            await transport.close()
            await fixture.server.stop()
        }
        try await connect(transport, fixture: fixture)
        XCTAssertTrue(fixture.server.pauseEventLoop())
        defer { fixture.server.resumeEventLoop() }

        let forward = SSHLocalForward(
            eventLoopGroup: transport.eventLoopGroup,
            destination: try SSHForwardDestination(
                host: "127.0.0.1",
                port: HermeticNIOSSHServer.echoDestinationPort
            ),
            connector: transport.makeDirectTCPIPConnector()
        )
        addTeardownBlock { await forward.close() }
        let localPort = try await forward.start()
        _ = try await ClientBootstrap(group: transport.eventLoopGroup)
            .connect(host: "127.0.0.1", port: Int(localPort))
            .get()

        // TCP connect completion can precede the listener's accepted-channel
        // initializer. Give that initializer one event-loop turn so this test
        // closes a genuinely pending direct-tcpip open.
        try await Task.sleep(for: .milliseconds(100))
        let start = ContinuousClock.now
        // Explicit listener teardown closes the accepted local channel even
        // though the server is withholding the SSH child-open response.
        await forward.close()
        try await waitUntilTransportFails(transport)
        XCTAssertLessThan(start.duration(to: .now), .seconds(2))
    }

    func testTerminalOutputBacklogFailsClosedAtHardLimit() async throws {
        let fixture = try await makeFixture(serverBehavior: .normal)
        let callbackQueue = DispatchQueue(
            label: "dev.wscrpt.tests.blocked-terminal-callback"
        )
        let blocker = DispatchSemaphore(value: 0)
        let didBlock = DispatchSemaphore(value: 0)
        callbackQueue.async {
            didBlock.signal()
            blocker.wait()
        }
        XCTAssertEqual(didBlock.wait(timeout: .now() + 1), .success)
        defer { blocker.signal() }

        let transport = SSHTransport(
            callbackQueue: callbackQueue,
            maximumPendingOutputBytes: 1_024
        )
        addTeardownBlock {
            await transport.close()
            await fixture.server.stop()
        }
        try await connect(transport, fixture: fixture)

        try await transport.send(Data("integration-terminal-flood".utf8))
        try await waitUntilTransportFails(transport)
        XCTAssertEqual(
            transport.state,
            .failed(
                SSHTransportError.terminalOutputBacklogExceeded
                    .localizedDescription
            )
        )
    }

    func testAlternatingTerminalStreamsHitChunkLimitBeforeCallbackFlood() async throws {
        let fixture = try await makeFixture(serverBehavior: .normal)
        let callbackQueue = DispatchQueue(
            label: "dev.wscrpt.tests.blocked-terminal-chunk-callback"
        )
        let blocker = DispatchSemaphore(value: 0)
        let didBlock = DispatchSemaphore(value: 0)
        callbackQueue.async {
            didBlock.signal()
            blocker.wait()
        }
        XCTAssertEqual(didBlock.wait(timeout: .now() + 1), .success)
        defer { blocker.signal() }

        let transport = SSHTransport(
            callbackQueue: callbackQueue,
            maximumPendingOutputBytes: 4_096,
            maximumPendingOutputChunks: 8
        )
        addTeardownBlock {
            await transport.close()
            await fixture.server.stop()
        }
        try await connect(transport, fixture: fixture)

        try await transport.send(
            Data("integration-terminal-alternating".utf8)
        )
        try await waitUntilTransportFails(transport)
        XCTAssertEqual(
            transport.state,
            .failed(
                SSHTransportError.terminalOutputBacklogExceeded
                    .localizedDescription
            )
        )
    }

    func testMismatchedPinnedHostKeyNeverReachesUserAuthentication() async throws {
        let identity = SSHEd25519Identity()
        let expectedClientKey = try NIOSSHPublicKey(
            openSSHPublicKey: identity.openSSHPublicKey
        )
        let serverState = HermeticSSHServerState()
        let server = HermeticNIOSSHServer(
            expectedUsername: "integration",
            expectedClientKey: expectedClientKey,
            state: serverState
        )
        let serverPort = try await server.start()
        let transport = SSHTransport()
        addTeardownBlock {
            await transport.close()
            await server.stop()
        }

        let endpoint = try SSHHostEndpoint(
            host: "127.0.0.1",
            port: serverPort
        )
        let unrelatedHostKey = NIOSSHPrivateKey(
            ed25519Key: Curve25519.Signing.PrivateKey()
        )
        let wrongFingerprint = try SSHHostKeyFingerprint(
            hostKey: unrelatedHostKey.publicKey
        )
        let trust = try StrictSSHHostKeyTrustDelegate(
            endpoint: endpoint,
            mode: .pinned(wrongFingerprint)
        )
        let configuration = try SSHConnectionConfiguration(
            endpoint: endpoint,
            username: "integration",
            credentials: [.ed25519(identity)],
            hostKeyTrust: trust,
            connectTimeoutSeconds: 5
        )

        var rejection: Error?
        do {
            try await transport.connect(
                configuration: configuration,
                initialSize: try SSHTerminalSize(columns: 80, rows: 24)
            )
            XCTFail("A mismatched pinned server key must never connect")
        } catch {
            rejection = error
        }

        XCTAssertNotNil(rejection)
        guard case .failed = transport.state else {
            XCTFail("A mismatched pinned server key must leave the transport failed")
            return
        }
        XCTAssertFalse(serverState.didReceiveAuthenticationRequest)
    }

    private struct Fixture {
        let identity: SSHEd25519Identity
        let server: HermeticNIOSSHServer
        let state: HermeticSSHServerState
        let endpoint: SSHHostEndpoint
        let trust: StrictSSHHostKeyTrustDelegate
    }

    private func makeFixture(
        serverBehavior: HermeticSSHServerBehavior
    ) async throws -> Fixture {
        let identity = SSHEd25519Identity()
        let expectedClientKey = try NIOSSHPublicKey(
            openSSHPublicKey: identity.openSSHPublicKey
        )
        let state = HermeticSSHServerState()
        let server = HermeticNIOSSHServer(
            expectedUsername: "integration",
            expectedClientKey: expectedClientKey,
            state: state,
            behavior: serverBehavior
        )
        let serverPort = try await server.start()
        let endpoint = try SSHHostEndpoint(
            host: "127.0.0.1",
            port: serverPort
        )
        let trust = try StrictSSHHostKeyTrustDelegate(
            endpoint: endpoint,
            mode: .pinned(
                try SSHHostKeyFingerprint(hostKey: server.hostPublicKey)
            )
        )
        return Fixture(
            identity: identity,
            server: server,
            state: state,
            endpoint: endpoint,
            trust: trust
        )
    }

    private func connect(
        _ transport: SSHTransport,
        fixture: Fixture
    ) async throws {
        try await transport.connect(
            configuration: try SSHConnectionConfiguration(
                endpoint: fixture.endpoint,
                username: "integration",
                credentials: [.ed25519(fixture.identity)],
                hostKeyTrust: fixture.trust,
                connectTimeoutSeconds: 5,
                handshakeTimeoutSeconds: 5
            ),
            initialSize: try SSHTerminalSize(columns: 80, rows: 24)
        )
    }

    private func waitUntilTransportFails(
        _ transport: SSHTransport,
        timeoutSeconds: TimeInterval = 3
    ) async throws {
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            if case .failed = transport.state {
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        throw HermeticSSHTestError.timedOut
    }

    private func waitUntil(
        timeoutSeconds: TimeInterval = 3,
        condition: () -> Bool
    ) async throws {
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            if condition() {
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        throw HermeticSSHTestError.timedOut
    }

    private func waitForFuture(
        _ future: EventLoopFuture<Void>,
        timeoutSeconds: Int
    ) async throws {
        let completion = future.eventLoop.makePromise(of: Void.self)
        let gate = HermeticOneShotGate()
        let timeout = future.eventLoop.scheduleTask(
            in: .seconds(Int64(timeoutSeconds))
        ) {
            guard gate.claim() else { return }
            completion.fail(HermeticSSHTestError.timedOut)
        }
        future.whenComplete { _ in
            guard gate.claim() else { return }
            timeout.cancel()
            // Callers assert lifetime completion, not the socket's close
            // reason. A transport-owned teardown commonly surfaces ECONNRESET.
            completion.succeed(())
        }
        try await completion.futureResult.get()
    }
}

private final class FailingHostKeyPinStore: SSHHostKeyPinStoring {
    private(set) var readCount = 0

    func pinnedFingerprint(
        for endpoint: SSHHostEndpoint
    ) throws -> SSHHostKeyFingerprint? {
        readCount += 1
        throw SSHHostKeyTrustError.corruptPinStore
    }

    func pinIfAbsent(
        _ fingerprint: SSHHostKeyFingerprint,
        for endpoint: SSHHostEndpoint
    ) throws -> SSHHostKeyFingerprint {
        throw SSHHostKeyTrustError.pinStoreFailure
    }

    func removePin(for endpoint: SSHHostEndpoint) throws {
        throw SSHHostKeyTrustError.pinStoreFailure
    }
}

private enum HermeticSSHTestError: Error {
    case invalidListener
    case unexpectedChannel
    case unexpectedChannelData
    case timedOut
}

private final class HermeticOneShotGate: @unchecked Sendable {
    private let lock = NSLock()
    private var isClaimed = false

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !isClaimed else { return false }
        isClaimed = true
        return true
    }
}

private enum HermeticSSHServerBehavior: Sendable {
    case normal
    case stallPTYReply
    case stallSecondSessionInitializer
    case holdSecondSessionInitializer
}

private struct HermeticTerminalSnapshot: Equatable {
    let columns: Int
    let rows: Int
    let pixelWidth: Int
    let pixelHeight: Int
}

private final class HermeticSSHServerState: @unchecked Sendable {
    private let lock = NSLock()
    private var authenticationRequestCountStorage = 0
    private var didAuthenticateExpectedKeyStorage = false
    private var didAuthenticateExpectedPasswordStorage = false
    private var initialPTYStorage: HermeticTerminalSnapshot?
    private var latestResizeStorage: HermeticTerminalSnapshot?
    private var shellInputStorage = Data()
    private var directTargetHostStorage: String?
    private var directTargetPortStorage: Int?
    private var openedSessionCount = 0
    private var heldSecondSessionInitializer: EventLoopPromise<Void>?
    private var isHoldingSecondSessionInitializerStorage = false
    private var execCommandsStorage: [String] = []

    var didReceiveAuthenticationRequest: Bool {
        withLock { authenticationRequestCountStorage > 0 }
    }

    var didAuthenticateExpectedKey: Bool {
        withLock { didAuthenticateExpectedKeyStorage }
    }

    var didAuthenticateExpectedPassword: Bool {
        withLock { didAuthenticateExpectedPasswordStorage }
    }

    var initialPTY: HermeticTerminalSnapshot? {
        withLock { initialPTYStorage }
    }

    var latestResize: HermeticTerminalSnapshot? {
        withLock { latestResizeStorage }
    }

    var shellInput: Data {
        withLock { shellInputStorage }
    }

    var directTargetHost: String? {
        withLock { directTargetHostStorage }
    }

    var directTargetPort: Int? {
        withLock { directTargetPortStorage }
    }

    var isHoldingSecondSessionInitializer: Bool {
        withLock { isHoldingSecondSessionInitializerStorage }
    }

    var execCommands: [String] {
        withLock { execCommandsStorage }
    }

    func recordAuthenticationRequest(accepted: Bool) {
        withLock {
            authenticationRequestCountStorage += 1
            didAuthenticateExpectedKeyStorage =
                didAuthenticateExpectedKeyStorage || accepted
        }
    }

    func recordPasswordAuthenticationRequest(accepted: Bool) {
        withLock {
            authenticationRequestCountStorage += 1
            didAuthenticateExpectedPasswordStorage =
                didAuthenticateExpectedPasswordStorage || accepted
        }
    }

    func recordPTY(_ request: SSHChannelRequestEvent.PseudoTerminalRequest) {
        withLock {
            initialPTYStorage = HermeticTerminalSnapshot(
                columns: request.terminalCharacterWidth,
                rows: request.terminalRowHeight,
                pixelWidth: request.terminalPixelWidth,
                pixelHeight: request.terminalPixelHeight
            )
        }
    }

    func recordResize(_ request: SSHChannelRequestEvent.WindowChangeRequest) {
        withLock {
            latestResizeStorage = HermeticTerminalSnapshot(
                columns: request.terminalCharacterWidth,
                rows: request.terminalRowHeight,
                pixelWidth: request.terminalPixelWidth,
                pixelHeight: request.terminalPixelHeight
            )
        }
    }

    func appendShellInput(_ data: Data) {
        withLock {
            shellInputStorage.append(data)
        }
    }

    func recordDirectTarget(host: String, port: Int) {
        withLock {
            directTargetHostStorage = host
            directTargetPortStorage = port
        }
    }

    func recordExecRequest(command: String) {
        withLock {
            execCommandsStorage.append(command)
        }
    }

    func nextSessionIndex() -> Int {
        withLock {
            openedSessionCount += 1
            return openedSessionCount
        }
    }

    func holdSecondSessionInitializer(
        on eventLoop: EventLoop
    ) -> EventLoopFuture<Void> {
        withLock {
            let promise = eventLoop.makePromise(of: Void.self)
            heldSecondSessionInitializer = promise
            isHoldingSecondSessionInitializerStorage = true
            return promise.futureResult
        }
    }

    func releaseHeldSessionInitializer() {
        let promise = withLock {
            let promise = heldSecondSessionInitializer
            heldSecondSessionInitializer = nil
            isHoldingSecondSessionInitializerStorage = false
            return promise
        }
        promise?.succeed(())
    }

    @discardableResult
    private func withLock<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }
}

private final class HermeticEd25519AuthDelegate:
    NIOSSHServerUserAuthenticationDelegate,
    @unchecked Sendable
{
    let supportedAuthenticationMethods:
        NIOSSHAvailableUserAuthenticationMethods = .publicKey

    private let expectedUsername: String
    private let expectedClientKey: NIOSSHPublicKey
    private let state: HermeticSSHServerState

    init(
        expectedUsername: String,
        expectedClientKey: NIOSSHPublicKey,
        state: HermeticSSHServerState
    ) {
        self.expectedUsername = expectedUsername
        self.expectedClientKey = expectedClientKey
        self.state = state
    }

    func requestReceived(
        request: NIOSSHUserAuthenticationRequest,
        responsePromise:
            EventLoopPromise<NIOSSHUserAuthenticationOutcome>
    ) {
        let accepted: Bool
        if request.username == expectedUsername,
           case let .publicKey(publicKeyRequest) = request.request,
           publicKeyRequest.publicKey == expectedClientKey {
            accepted = true
        } else {
            accepted = false
        }
        state.recordAuthenticationRequest(accepted: accepted)
        responsePromise.succeed(accepted ? .success : .failure)
    }
}

private final class HermeticPasswordAuthDelegate:
    NIOSSHServerUserAuthenticationDelegate,
    @unchecked Sendable
{
    let supportedAuthenticationMethods:
        NIOSSHAvailableUserAuthenticationMethods = .password

    private let expectedUsername: String
    private let expectedPassword: String
    private let state: HermeticSSHServerState

    init(
        expectedUsername: String,
        expectedPassword: String,
        state: HermeticSSHServerState
    ) {
        self.expectedUsername = expectedUsername
        self.expectedPassword = expectedPassword
        self.state = state
    }

    func requestReceived(
        request: NIOSSHUserAuthenticationRequest,
        responsePromise:
            EventLoopPromise<NIOSSHUserAuthenticationOutcome>
    ) {
        let accepted: Bool
        if request.username == expectedUsername,
           case let .password(passwordRequest) = request.request,
           passwordRequest.password == expectedPassword {
            accepted = true
        } else {
            accepted = false
        }
        state.recordPasswordAuthenticationRequest(accepted: accepted)
        responsePromise.succeed(accepted ? .success : .failure)
    }
}

private final class HermeticNIOSSHServer: @unchecked Sendable {
    static let echoDestinationPort = 7_331

    private let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
    private let hostKey = NIOSSHPrivateKey(
        ed25519Key: Curve25519.Signing.PrivateKey()
    )
    private let authDelegate: NIOSSHServerUserAuthenticationDelegate
    private let state: HermeticSSHServerState
    private let behavior: HermeticSSHServerBehavior
    private let lifecycleLock = NSLock()
    private var listener: Channel?
    private var didShutDown = false
    private var eventLoopBlocker: DispatchSemaphore?

    var hostPublicKey: NIOSSHPublicKey {
        hostKey.publicKey
    }

    init(
        expectedUsername: String,
        expectedClientKey: NIOSSHPublicKey,
        state: HermeticSSHServerState,
        behavior: HermeticSSHServerBehavior = .normal
    ) {
        authDelegate = HermeticEd25519AuthDelegate(
            expectedUsername: expectedUsername,
            expectedClientKey: expectedClientKey,
            state: state
        )
        self.state = state
        self.behavior = behavior
    }

    init(
        expectedUsername: String,
        expectedPassword: String,
        state: HermeticSSHServerState,
        behavior: HermeticSSHServerBehavior = .normal
    ) {
        authDelegate = HermeticPasswordAuthDelegate(
            expectedUsername: expectedUsername,
            expectedPassword: expectedPassword,
            state: state
        )
        self.state = state
        self.behavior = behavior
    }

    func start() async throws -> Int {
        let serverConfiguration = HermeticUncheckedSendableBox(
            SSHServerConfiguration(
                hostKeys: [hostKey],
                userAuthDelegate: authDelegate
            )
        )
        let state = state
        let behavior = behavior
        let channel: Channel
        do {
            channel = try await ServerBootstrap(group: group)
                .serverChannelOption(
                    ChannelOptions.socketOption(.so_reuseaddr),
                    value: 1
                )
                .childChannelOption(
                    ChannelOptions.socketOption(.tcp_nodelay),
                    value: 1
                )
                .childChannelInitializer { channel in
                    let ssh = NIOSSHHandler(
                        role: .server(serverConfiguration.value),
                        allocator: channel.allocator
                    ) { child, channelType in
                        switch channelType {
                        case .session:
                            let sessionIndex = state.nextSessionIndex()
                            if behavior == .stallSecondSessionInitializer,
                               sessionIndex == 2 {
                                return child.eventLoop.makePromise(of: Void.self)
                                    .futureResult
                            }
                            if behavior == .holdSecondSessionInitializer,
                               sessionIndex == 2 {
                                return state.holdSecondSessionInitializer(
                                    on: child.eventLoop
                                ).flatMap {
                                    child.pipeline.addHandler(
                                        HermeticSSHSessionHandler(state: state)
                                    )
                                }
                            }
                            return child.pipeline.addHandler(
                                HermeticSSHSessionHandler(
                                    state: state,
                                    stallPTYReply: behavior == .stallPTYReply
                                )
                            )

                        case let .directTCPIP(target):
                            guard target.targetHost == "127.0.0.1",
                                  target.targetPort
                                    == HermeticNIOSSHServer.echoDestinationPort
                            else {
                                return child.eventLoop.makeFailedFuture(
                                    HermeticSSHTestError.unexpectedChannel
                                )
                            }
                            state.recordDirectTarget(
                                host: target.targetHost,
                                port: target.targetPort
                            )
                            return child.pipeline.addHandler(
                                HermeticDirectEchoHandler()
                            )

                        case .forwardedTCPIP:
                            return child.eventLoop.makeFailedFuture(
                                HermeticSSHTestError.unexpectedChannel
                            )
                        }
                    }
                    return channel.eventLoop.makeCompletedFuture {
                        let pipeline = channel.pipeline.syncOperations
                        try pipeline.addHandler(ssh)
                        try pipeline.addHandler(HermeticSSHRootErrorHandler())
                    }
                }
                .bind(host: "127.0.0.1", port: 0)
                .get()
        } catch {
            await shutdownGroup()
            throw error
        }

        guard let address = channel.localAddress,
              address.ipAddress == "127.0.0.1",
              let port = address.port,
              port > 0
        else {
            try? await channel.close()
            await shutdownGroup()
            throw HermeticSSHTestError.invalidListener
        }
        withLifecycleLock {
            listener = channel
        }
        return port
    }

    func stop() async {
        state.releaseHeldSessionInitializer()
        resumeEventLoop()
        let channel = withLifecycleLock {
            let channel = listener
            listener = nil
            return channel
        }
        try? await channel?.close()
        await shutdownGroup()
    }

    /// Blocks the hermetic server's sole event loop after a successful
    /// terminal connection. A later client child-open request therefore gets
    /// no protocol response until `resumeEventLoop`, exercising the client's
    /// deadline instead of merely delaying a server-side initializer.
    func pauseEventLoop() -> Bool {
        let blocker = DispatchSemaphore(value: 0)
        let entered = DispatchSemaphore(value: 0)
        lifecycleLock.lock()
        guard eventLoopBlocker == nil, !didShutDown else {
            lifecycleLock.unlock()
            return false
        }
        eventLoopBlocker = blocker
        lifecycleLock.unlock()

        group.next().execute {
            entered.signal()
            blocker.wait()
        }
        return entered.wait(timeout: .now() + 1) == .success
    }

    func resumeEventLoop() {
        lifecycleLock.lock()
        let blocker = eventLoopBlocker
        eventLoopBlocker = nil
        lifecycleLock.unlock()
        blocker?.signal()
    }

    func releaseHeldSessionInitializer() {
        state.releaseHeldSessionInitializer()
    }

    private func shutdownGroup() async {
        let shouldShutDown = withLifecycleLock {
            guard !didShutDown else { return false }
            didShutDown = true
            return true
        }
        guard shouldShutDown else { return }

        await withCheckedContinuation { continuation in
            group.shutdownGracefully { _ in
                continuation.resume()
            }
        }
    }

    private func withLifecycleLock<T>(_ body: () throws -> T) rethrows -> T {
        lifecycleLock.lock()
        defer { lifecycleLock.unlock() }
        return try body()
    }
}

private final class HermeticUncheckedSendableBox<Value>: @unchecked Sendable {
    let value: Value

    init(_ value: Value) {
        self.value = value
    }
}

private final class HermeticSSHRootErrorHandler: ChannelInboundHandler {
    typealias InboundIn = Any

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        context.close(promise: nil)
    }
}

private final class HermeticSSHSessionHandler: ChannelDuplexHandler {
    typealias InboundIn = SSHChannelData
    typealias OutboundIn = SSHChannelData
    typealias OutboundOut = SSHChannelData

    private let state: HermeticSSHServerState
    private let stallPTYReply: Bool
    private var didStartShell = false
    private var didStartExec = false

    init(
        state: HermeticSSHServerState,
        stallPTYReply: Bool = false
    ) {
        self.state = state
        self.stallPTYReply = stallPTYReply
    }

    func userInboundEventTriggered(
        context: ChannelHandlerContext,
        event: Any
    ) {
        switch event {
        case let request as SSHChannelRequestEvent.PseudoTerminalRequest:
            state.recordPTY(request)
            guard !stallPTYReply else { return }
            replyIfRequested(
                request.wantReply,
                context: context
            )

        case let request as SSHChannelRequestEvent.ShellRequest:
            guard !didStartShell, !didStartExec else {
                context.triggerUserOutboundEvent(
                    ChannelFailureEvent(),
                    promise: nil
                )
                return
            }
            didStartShell = true
            replyIfRequested(request.wantReply, context: context)
            write(
                "shell-ready",
                stream: .channel,
                context: context
            )

        case let request as SSHChannelRequestEvent.ExecRequest:
            guard !didStartShell, !didStartExec else {
                context.triggerUserOutboundEvent(
                    ChannelFailureEvent(),
                    promise: nil
                )
                return
            }
            didStartExec = true
            state.recordExecRequest(command: request.command)
            replyIfRequested(request.wantReply, context: context)
            completeExec(request.command, context: context)

        case let request as SSHChannelRequestEvent.WindowChangeRequest:
            state.recordResize(request)
            write(
                "resize:\(request.terminalCharacterWidth)x\(request.terminalRowHeight)",
                stream: .channel,
                context: context
            )

        default:
            context.fireUserInboundEventTriggered(event)
        }
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        guard didStartShell else {
            context.fireErrorCaught(HermeticSSHTestError.unexpectedChannelData)
            return
        }
        let channelData = unwrapInboundIn(data)
        guard channelData.type == .channel,
              case let .byteBuffer(buffer) = channelData.data
        else {
            context.fireErrorCaught(HermeticSSHTestError.unexpectedChannelData)
            return
        }

        let bytes = Data(buffer.readableBytesView)
        state.appendShellInput(bytes)
        if bytes == Data("integration-terminal-flood".utf8) {
            var flood = context.channel.allocator.buffer(capacity: 4_096)
            flood.writeRepeatingByte(UInt8(ascii: "x"), count: 4_096)
            context.writeAndFlush(
                wrapOutboundOut(
                    SSHChannelData(
                        type: .channel,
                        data: .byteBuffer(flood)
                    )
                ),
                promise: nil
            )
            return
        }
        if bytes == Data("integration-terminal-alternating".utf8) {
            for index in 0 ..< 64 {
                var byte = context.channel.allocator.buffer(capacity: 1)
                byte.writeInteger(UInt8(index & 0xff))
                context.write(
                    wrapOutboundOut(
                        SSHChannelData(
                            type: index.isMultiple(of: 2)
                                ? .channel
                                : .stdErr,
                            data: .byteBuffer(byte)
                        )
                    ),
                    promise: nil
                )
            }
            context.flush()
            return
        }
        var response = context.channel.allocator.buffer(
            capacity: 6 + bytes.count
        )
        response.writeString("shell:")
        response.writeBytes(bytes)
        context.writeAndFlush(
            wrapOutboundOut(
                SSHChannelData(
                    type: .channel,
                    data: .byteBuffer(response)
                )
            ),
            promise: nil
        )
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        context.close(promise: nil)
    }

    private func replyIfRequested(
        _ requested: Bool,
        context: ChannelHandlerContext
    ) {
        if requested {
            context.triggerUserOutboundEvent(
                ChannelSuccessEvent(),
                promise: nil
            )
        }
    }

    private func completeExec(
        _ command: String,
        context: ChannelHandlerContext
    ) {
        switch command {
        case "integration-ok":
            sendExecResult(
                standardOutput: "exec-stdout",
                standardError: "exec-stderr",
                exitStatus: 0,
                context: context
            )

        case "integration-flood":
            sendExecResult(
                standardOutput: String(repeating: "x", count: 64),
                standardError: nil,
                exitStatus: 0,
                context: context
            )

        default:
            sendExecResult(
                standardOutput: nil,
                standardError: "unsupported",
                exitStatus: 127,
                context: context
            )
        }
    }

    private func write(
        _ string: String,
        stream: SSHChannelData.DataType,
        flush: Bool = true,
        context: ChannelHandlerContext
    ) {
        var buffer = context.channel.allocator.buffer(
            capacity: string.utf8.count
        )
        buffer.writeString(string)
        let wrapped = wrapOutboundOut(
            SSHChannelData(
                type: stream,
                data: .byteBuffer(buffer)
            )
        )
        if flush {
            context.writeAndFlush(wrapped, promise: nil)
        } else {
            context.write(wrapped, promise: nil)
        }
    }

    private func sendExecResult(
        standardOutput: String?,
        standardError: String?,
        exitStatus: Int,
        context: ChannelHandlerContext
    ) {
        let payloads: [(String, SSHChannelData.DataType)] = [
            standardOutput.map { ($0, .channel) },
            standardError.map { ($0, .stdErr) },
        ].compactMap { $0 }

        guard let last = payloads.last else {
            context.triggerUserOutboundEvent(
                SSHChannelRequestEvent.ExitStatus(exitStatus: exitStatus)
            ).whenComplete { _ in
                context.close(promise: nil)
            }
            return
        }

        for payload in payloads.dropLast() {
            context.write(
                makeOutboundData(
                    payload.0,
                    stream: payload.1,
                    context: context
                ),
                promise: nil
            )
        }

        let flushPromise = context.eventLoop.makePromise(of: Void.self)
        context.writeAndFlush(
            makeOutboundData(
                last.0,
                stream: last.1,
                context: context
            ),
            promise: flushPromise
        )
        flushPromise.futureResult.whenComplete { result in
            switch result {
            case .success:
                context.triggerUserOutboundEvent(
                    SSHChannelRequestEvent.ExitStatus(
                        exitStatus: exitStatus
                    )
                ).whenComplete { _ in
                    context.close(promise: nil)
                }
            case .failure:
                context.close(promise: nil)
            }
        }
    }

    private func makeOutboundData(
        _ string: String,
        stream: SSHChannelData.DataType,
        context: ChannelHandlerContext
    ) -> NIOAny {
        var buffer = context.channel.allocator.buffer(
            capacity: string.utf8.count
        )
        buffer.writeString(string)
        return wrapOutboundOut(
            SSHChannelData(
                type: stream,
                data: .byteBuffer(buffer)
            )
        )
    }
}

private final class HermeticDirectEchoHandler: ChannelDuplexHandler {
    typealias InboundIn = SSHChannelData
    typealias OutboundIn = SSHChannelData
    typealias OutboundOut = SSHChannelData

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let channelData = unwrapInboundIn(data)
        guard channelData.type == .channel,
              case .byteBuffer = channelData.data
        else {
            context.fireErrorCaught(HermeticSSHTestError.unexpectedChannelData)
            return
        }
        context.writeAndFlush(
            wrapOutboundOut(channelData),
            promise: nil
        )
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        context.close(promise: nil)
    }
}

private final class HermeticTCPResponseHandler: ChannelInboundHandler {
    typealias InboundIn = ByteBuffer

    private let expectedByteCount: Int
    private var responsePromise: EventLoopPromise<Data>?
    private var received = Data()

    init(
        expectedByteCount: Int,
        responsePromise: EventLoopPromise<Data>
    ) {
        self.expectedByteCount = expectedByteCount
        self.responsePromise = responsePromise
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let buffer = unwrapInboundIn(data)
        received.append(contentsOf: buffer.readableBytesView)
        guard received.count >= expectedByteCount,
              let responsePromise
        else {
            return
        }
        self.responsePromise = nil
        responsePromise.succeed(
            Data(received.prefix(expectedByteCount))
        )
    }

    func channelInactive(context: ChannelHandlerContext) {
        if let responsePromise {
            self.responsePromise = nil
            responsePromise.fail(SSHTransportError.connectionClosed)
        }
        context.fireChannelInactive()
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        if let responsePromise {
            self.responsePromise = nil
            responsePromise.fail(error)
        }
        context.close(promise: nil)
    }
}

private final class HermeticOutputRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var received = Data()

    func append(_ data: Data) {
        withLock {
            received.append(data)
        }
    }

    func waitUntilContains(
        _ marker: String,
        timeoutSeconds: TimeInterval = 5
    ) async throws {
        let expected = Data(marker.utf8)
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            let found = withLock {
                received.range(of: expected) != nil
            }
            if found {
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        throw HermeticSSHTestError.timedOut
    }

    private func withLock<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }
}
