import CryptoKit
import Dispatch
import Foundation
import NIOCore
import NIOPosix
import NIOSSH

/// An app-generated Ed25519 client identity. The raw private bytes are exposed
/// only so the app can persist them in a secret store such as Keychain.
struct SSHEd25519Identity: Sendable {
    private let signingKey: Curve25519.Signing.PrivateKey

    init() {
        signingKey = Curve25519.Signing.PrivateKey()
    }

    init(rawPrivateKeyRepresentation: Data) throws {
        guard rawPrivateKeyRepresentation.count == 32 else {
            throw SSHTransportError.invalidIdentity
        }
        do {
            signingKey = try Curve25519.Signing.PrivateKey(
                rawRepresentation: rawPrivateKeyRepresentation
            )
        } catch {
            throw SSHTransportError.invalidIdentity
        }
    }

    static func loadOrCreate(
        secretStore: SecretStoring,
        account: String
    ) throws -> SSHEd25519Identity {
        if let stored = try secretStore.data(for: account) {
            return try SSHEd25519Identity(rawPrivateKeyRepresentation: stored)
        }

        let identity = SSHEd25519Identity()
        try secretStore.set(identity.rawPrivateKeyRepresentation, for: account)
        return identity
    }

    var rawPrivateKeyRepresentation: Data {
        signingKey.rawRepresentation
    }

    var openSSHPublicKey: String {
        String(openSSHPublicKey: nioPrivateKey.publicKey)
    }

    fileprivate var nioPrivateKey: NIOSSHPrivateKey {
        NIOSSHPrivateKey(ed25519Key: signingKey)
    }
}

enum SSHAuthenticationCredential: Sendable {
    case ed25519(SSHEd25519Identity)
    case password(String)
}

struct SSHConnectionConfiguration: Sendable {
    let endpoint: SSHHostEndpoint
    let username: String
    let credentials: [SSHAuthenticationCredential]
    let hostKeyTrust: StrictSSHHostKeyTrustDelegate
    let connectTimeout: TimeAmount
    let handshakeTimeout: TimeAmount

    init(
        endpoint: SSHHostEndpoint,
        username: String,
        credentials: [SSHAuthenticationCredential],
        hostKeyTrust: StrictSSHHostKeyTrustDelegate,
        connectTimeoutSeconds: Int = 20,
        handshakeTimeoutSeconds: Int = 120
    ) throws {
        guard !username.isEmpty,
              username.utf8.count <= 256,
              username.unicodeScalars.allSatisfy({
                  !CharacterSet.controlCharacters.contains($0)
              }),
              hostKeyTrust.trustedEndpoint == endpoint,
              !credentials.isEmpty,
              credentials.count <= 8,
              (1 ... 120).contains(connectTimeoutSeconds),
              (1 ... 600).contains(handshakeTimeoutSeconds)
        else {
            throw SSHTransportError.invalidConfiguration
        }

        for credential in credentials {
            if case let .password(password) = credential,
               password.isEmpty || password.utf8.count > 16_384
            {
                throw SSHTransportError.invalidConfiguration
            }
        }

        self.endpoint = endpoint
        self.username = username
        self.credentials = credentials
        self.hostKeyTrust = hostKeyTrust
        connectTimeout = .seconds(Int64(connectTimeoutSeconds))
        handshakeTimeout = .seconds(Int64(handshakeTimeoutSeconds))
    }
}

struct SSHTerminalSize: Equatable, Sendable {
    let columns: Int
    let rows: Int
    let pixelWidth: Int
    let pixelHeight: Int

    init(
        columns: Int,
        rows: Int,
        pixelWidth: Int = 0,
        pixelHeight: Int = 0
    ) throws {
        guard (1 ... 65_535).contains(columns),
              (1 ... 65_535).contains(rows),
              (0 ... Int(UInt32.max)).contains(pixelWidth),
              (0 ... Int(UInt32.max)).contains(pixelHeight)
        else {
            throw SSHTransportError.invalidTerminalSize
        }
        self.columns = columns
        self.rows = rows
        self.pixelWidth = pixelWidth
        self.pixelHeight = pixelHeight
    }
}

struct SSHExecLimits: Equatable, Sendable {
    let maximumOutputBytes: Int
    let timeout: TimeAmount

    init(
        maximumOutputBytes: Int = 1_048_576,
        timeoutSeconds: Int = 30
    ) throws {
        guard (1 ... 16_777_216).contains(maximumOutputBytes),
              (1 ... 600).contains(timeoutSeconds)
        else {
            throw SSHTransportError.invalidExecLimits
        }
        self.maximumOutputBytes = maximumOutputBytes
        timeout = .seconds(Int64(timeoutSeconds))
    }
}

enum SSHOutputStream: Equatable, Sendable {
    case standardOutput
    case standardError
}

struct SSHOutputChunk: Equatable, Sendable {
    let stream: SSHOutputStream
    var data: Data
}

struct SSHExecResult: Equatable, Sendable {
    let exitStatus: Int
    let standardOutput: Data
    let standardError: Data
}

enum SSHTransportState: Equatable, Sendable {
    case idle
    case connecting
    case connected
    case closing
    case closed
    case failed(String)
}

struct SSHTransportCallbacks {
    var onOutput: (SSHOutputChunk) -> Void
    var onStateChange: (SSHTransportState) -> Void

    init(
        onOutput: @escaping (SSHOutputChunk) -> Void = { _ in },
        onStateChange: @escaping (SSHTransportState) -> Void = { _ in }
    ) {
        self.onOutput = onOutput
        self.onStateChange = onStateChange
    }
}

/// A single SSH connection that owns one interactive PTY plus bounded exec and
/// direct-tcpip child channels. Public callbacks are delivered on
/// `callbackQueue`; all NIO handler work remains on the connection event loop.
final class SSHTransport: @unchecked Sendable {
    static let maximumInputChunkBytes = 65_536
    static let maximumPendingTerminalOutputBytes = 4_194_304
    static let maximumPendingTerminalOutputChunks = 4_096

    private static let defaultDirectOpenTimeout: TimeAmount = .seconds(15)

    private let callbackQueue: DispatchQueue
    private let callbacks: SSHTransportCallbacks
    private let maximumPendingOutputBytes: Int
    private let maximumPendingOutputChunks: Int
    private let directOpenTimeout: TimeAmount
    private let lock = NSLock()
    let eventLoopGroup: EventLoopGroup
    private let ownedEventLoopGroup: MultiThreadedEventLoopGroup

    private var stateStorage: SSHTransportState = .idle
    private var generation: UInt64 = 0
    private var rootChannel: Channel?
    private var terminalChannel: Channel?
    private var execChannels: [UUID: Channel] = [:]
    private var forwardChannels: [UUID: Channel] = [:]
    private var pendingOutput: [SSHOutputChunk] = []
    private var pendingOutputBytes = 0
    private var pendingOutputChunkCount = 0
    private var isOutputDrainScheduled = false
    private var isFailureTeardownScheduled = false

    init(
        callbackQueue: DispatchQueue = .main,
        callbacks: SSHTransportCallbacks = .init(),
        maximumPendingOutputBytes: Int = SSHTransport.maximumPendingTerminalOutputBytes,
        maximumPendingOutputChunks: Int = SSHTransport.maximumPendingTerminalOutputChunks,
        directOpenTimeout: TimeAmount = SSHTransport.defaultDirectOpenTimeout
    ) {
        self.callbackQueue = callbackQueue
        self.callbacks = callbacks
        self.maximumPendingOutputBytes = max(1, maximumPendingOutputBytes)
        self.maximumPendingOutputChunks = max(1, maximumPendingOutputChunks)
        self.directOpenTimeout = directOpenTimeout
        let group = MultiThreadedEventLoopGroup(numberOfThreads: 1)
        ownedEventLoopGroup = group
        eventLoopGroup = group
    }

    var state: SSHTransportState {
        withLock { stateStorage }
    }

    var sharedEventLoopGroup: EventLoopGroup? {
        withLock {
            guard stateStorage == .connected else { return nil }
            return eventLoopGroup
        }
    }

    func connect(
        configuration: SSHConnectionConfiguration,
        initialSize: SSHTerminalSize,
        terminalType: String = "xterm-256color"
    ) async throws {
        guard Self.isValidTerminalType(terminalType) else {
            throw SSHTransportError.invalidTerminalType
        }

        let group = ownedEventLoopGroup
        let currentGeneration: UInt64
        do {
            currentGeneration = try withLock {
                switch stateStorage {
                case .idle, .closed, .failed:
                    break
                case .connecting, .connected, .closing:
                    throw SSHTransportError.alreadyConnected
                }

                generation &+= 1
                rootChannel = nil
                terminalChannel = nil
                execChannels.removeAll(keepingCapacity: false)
                forwardChannels.removeAll(keepingCapacity: false)
                resetPendingOutputLocked()
                isFailureTeardownScheduled = false
                stateStorage = .connecting
                return generation
            }
        } catch {
            throw error
        }
        emitState(.connecting)

        let userAuth = SequentialSSHAuthenticationDelegate(
            username: configuration.username,
            credentials: configuration.credentials
        )

        let bootstrap = ClientBootstrap(group: group)
            .connectTimeout(configuration.connectTimeout)
            .channelInitializer { [weak self] channel in
                let sshHandler = NIOSSHHandler(
                    role: .client(
                        .init(
                            userAuthDelegate: userAuth,
                            serverAuthDelegate: configuration.hostKeyTrust
                        )
                    ),
                    allocator: channel.allocator,
                    inboundChildChannelInitializer: nil
                )
                let lifecycle = SSHRootLifecycleHandler(
                    onError: { [weak self] error in
                        self?.unexpectedConnectionEnd(
                            error: error,
                            generation: currentGeneration
                        )
                    },
                    onInactive: { [weak self] in
                        self?.unexpectedConnectionEnd(
                            error: SSHTransportError.connectionClosed,
                            generation: currentGeneration
                        )
                    }
                )
                return channel.eventLoop.makeCompletedFuture {
                    let pipeline = channel.pipeline.syncOperations
                    try pipeline.addHandler(sshHandler)
                    try pipeline.addHandler(lifecycle)
                }
            }

        do {
            let root = try await bootstrap.connect(
                host: configuration.endpoint.host,
                port: configuration.endpoint.port
            ).get()
            guard registerRoot(root, generation: currentGeneration) else {
                try? await root.close()
                throw SSHTransportError.connectionCancelled
            }

            // TCP establishment has its own short timeout. Authentication,
            // host-key confirmation, child creation, PTY, and shell setup use
            // a separate, deliberately longer deadline so a first-use native
            // fingerprint prompt remains practical while the connect attempt
            // is still bounded.
            let handshakeDeadline = NIODeadline.now()
                + configuration.handshakeTimeout

            let terminalReady = SSHOneShotVoidPromise(
                eventLoop: root.eventLoop
            )
            let terminalOpen = Self.createChildChannel(
                on: root,
                channelType: .session
            ) { [weak self] childChannel, channelType in
                guard channelType == .session else {
                    return childChannel.eventLoop.makeFailedFuture(
                        SSHTransportError.invalidChannelType
                    )
                }

                let handler = SSHPTYChannelHandler(
                    initialSize: initialSize,
                    terminalType: terminalType,
                    readyCompletion: terminalReady,
                    onOutput: { [weak self] chunk in
                        self?.emitOutput(chunk, generation: currentGeneration)
                    },
                    onClosed: { [weak self] error in
                        self?.unexpectedConnectionEnd(
                            error: error ?? SSHTransportError.sessionClosed,
                            generation: currentGeneration
                        )
                    }
                )
                return childChannel.eventLoop.makeCompletedFuture {
                    try childChannel.pipeline.syncOperations.addHandler(handler)
                }
            }
            let terminalFuture = Self.future(
                terminalOpen,
                boundedBy: handshakeDeadline,
                timeoutError: SSHTransportError.connectionTimedOut,
                onTimeout: {
                    root.close(promise: nil)
                },
                onLateSuccess: { channel in
                    channel.close(promise: nil)
                }
            )
            let terminal: Channel
            do {
                terminal = try await terminalFuture.get()
            } catch {
                terminalReady.fail(error)
                throw error
            }
            guard registerTerminal(terminal, generation: currentGeneration) else {
                terminalReady.fail(SSHTransportError.connectionCancelled)
                try? await terminal.close()
                throw SSHTransportError.connectionCancelled
            }
            try await Self.future(
                terminalReady.futureResult,
                boundedBy: handshakeDeadline,
                timeoutError: SSHTransportError.connectionTimedOut,
                onTimeout: {
                    terminalReady.fail(SSHTransportError.connectionTimedOut)
                    terminal.close(promise: nil)
                    root.close(promise: nil)
                }
            ).get()

            let didConnect = withLock {
                guard generation == currentGeneration,
                      stateStorage == .connecting
                else {
                    return false
                }
                stateStorage = .connected
                return true
            }
            guard didConnect else {
                throw SSHTransportError.connectionCancelled
            }
            emitState(.connected)
        } catch {
            await failAndTearDown(error: error, generation: currentGeneration)
            throw error
        }
    }

    func send(_ data: Data) async throws {
        guard !data.isEmpty else { return }
        guard data.count <= Self.maximumInputChunkBytes else {
            throw SSHTransportError.inputChunkTooLarge
        }
        let channel = try connectedTerminalChannel()
        var buffer = channel.allocator.buffer(capacity: data.count)
        buffer.writeBytes(data)
        try await channel.writeAndFlush(
            SSHChannelData(type: .channel, data: .byteBuffer(buffer))
        ).get()
    }

    func resize(_ size: SSHTerminalSize) async throws {
        let channel = try connectedTerminalChannel()
        let request = SSHChannelRequestEvent.WindowChangeRequest(
            terminalCharacterWidth: size.columns,
            terminalRowHeight: size.rows,
            terminalPixelWidth: size.pixelWidth,
            terminalPixelHeight: size.pixelHeight
        )
        try await channel.triggerUserOutboundEvent(request).get()
    }

    func execute(
        _ command: String,
        limits: SSHExecLimits
    ) async throws -> SSHExecResult {
        guard !command.isEmpty,
              command.utf8.count <= 32_768,
              !command.unicodeScalars.contains(where: { $0.value == 0 })
        else {
            throw SSHTransportError.invalidCommand
        }

        try Task.checkCancellation()
        let (root, currentGeneration) = try connectedRootSnapshot()
        let identifier = UUID()
        let cancellationGate = SSHExecCancellationGate()
        let resultPromise = root.eventLoop.makePromise(of: SSHExecResult.self)
        let deadline = NIODeadline.now() + limits.timeout
        let childOpen = Self.createChildChannel(
            on: root,
            channelType: .session
        ) { childChannel, channelType in
            guard channelType == .session else {
                return childChannel.eventLoop.makeFailedFuture(
                    SSHTransportError.invalidChannelType
                )
            }
            guard cancellationGate.register(childChannel) else {
                return childChannel.eventLoop.makeFailedFuture(
                    CancellationError()
                )
            }
            return childChannel.eventLoop.makeCompletedFuture {
                try childChannel.pipeline.syncOperations.addHandler(
                    SSHExecChannelHandler(
                        command: command,
                        maximumOutputBytes: limits.maximumOutputBytes,
                        resultPromise: resultPromise,
                        cancellationGate: cancellationGate
                    )
                )
            }
        }
        do {
            let channel = try await Self.cancellableValue(
                childOpen,
                boundedBy: deadline,
                timeoutError: SSHTransportError.execTimedOut,
                onTimeout: {
                    // NIOSSH does not expose cancellation for a child-open
                    // request. Closing the root is the only way to bound a
                    // server that never answers and guarantees no late child
                    // is orphaned.
                    root.close(promise: nil)
                },
                onCancel: {
                    cancellationGate.requestTaskCancellation()
                },
                onLateSuccess: { channel in
                    channel.close(promise: nil)
                },
                checkCancellationAfterValue: false
            )
            guard registerExec(
                channel,
                identifier: identifier,
                generation: currentGeneration
            ) else {
                cancellationGate.forceCancel()
                try? await channel.close()
                throw SSHTransportError.connectionCancelled
            }

            defer {
                unregisterExec(identifier: identifier)
                cancellationGate.clear(channel)
            }
            return try await Self.cancellableValue(
                resultPromise.futureResult,
                boundedBy: deadline,
                timeoutError: SSHTransportError.execTimedOut,
                onTimeout: {
                    // Once ExecRequest has begun, closing only the child does
                    // not prove that the remote command stopped. Tear down the
                    // SSH connection so a timed-out mutating command cannot
                    // race a replacement command on the same transport.
                    root.close(promise: nil)
                },
                onCancel: {
                    cancellationGate.requestTaskCancellation()
                }
            )
        } catch {
            cancellationGate.forceCancel()
            throw error
        }
    }

    func runCommand(
        _ command: String,
        maximumOutputBytes: Int
    ) async throws -> Data {
        let limits = try SSHExecLimits(maximumOutputBytes: maximumOutputBytes)
        let result = try await execute(command, limits: limits)
        guard result.exitStatus == 0 else {
            throw SSHTransportError.commandFailed(exitStatus: result.exitStatus)
        }
        return result.standardOutput
    }

    /// Produces the connector required by `SSHLocalForward`. The loopback
    /// listener must use `sharedEventLoopGroup`; the single event-loop design
    /// makes the paired glue handlers deterministic and avoids cross-loop races.
    func makeDirectTCPIPConnector() -> SSHDirectTCPIPConnector {
        { [weak self] localChannel, destination in
            guard let self else {
                return localChannel.eventLoop.makeFailedFuture(
                    SSHTransportError.connectionClosed
                )
            }
            return self.connectDirectTCPIP(
                localChannel: localChannel,
                destination: destination
            )
        }
    }

    func close() async {
        let resources: TransportResources?
        let shouldEmitClosing: Bool
        (resources, shouldEmitClosing) = withLock {
            switch stateStorage {
            case .idle, .closed:
                stateStorage = .closed
                return (nil, false)
            case .closing:
                return (nil, false)
            case .connecting, .connected, .failed:
                stateStorage = .closing
                generation &+= 1
                return (takeResourcesLocked(), true)
            }
        }

        if shouldEmitClosing {
            emitState(.closing)
        }
        if let resources {
            await Self.closeResources(resources)
        }

        let didClose = withLock {
            guard stateStorage == .closing || stateStorage == .idle else {
                return stateStorage == .closed
            }
            stateStorage = .closed
            return true
        }
        if didClose {
            emitState(.closed)
        }
    }

    deinit {
        let resources = withLock { takeResourcesLocked() }
        let group = ownedEventLoopGroup
        for channel in resources?.execChannels ?? [] {
            channel.close(promise: nil)
        }
        for channel in resources?.forwardChannels ?? [] {
            channel.close(promise: nil)
        }
        resources?.terminalChannel?.close(promise: nil)
        if let root = resources?.rootChannel {
            root.close().whenComplete { _ in
                group.shutdownGracefully { _ in }
            }
        } else {
            group.shutdownGracefully { _ in }
        }
    }

    func connectDirectTCPIP(
        localChannel: Channel,
        destination: SSHForwardDestination
    ) -> EventLoopFuture<Void> {
        let root: Channel
        let currentGeneration: UInt64
        do {
            (root, currentGeneration) = try connectedRootSnapshot()
        } catch {
            return localChannel.eventLoop.makeFailedFuture(error)
        }
        guard root.eventLoop === localChannel.eventLoop else {
            return localChannel.eventLoop.makeFailedFuture(
                SSHTransportError.forwardRequiresSharedEventLoop
            )
        }
        guard let originatorAddress = localChannel.remoteAddress else {
            return localChannel.eventLoop.makeFailedFuture(
                SSHTransportError.missingOriginatorAddress
            )
        }

        let channelPromise = root.eventLoop.makePromise(of: Channel.self)
        let identifier = UUID()
        let directTCPIP = SSHChannelType.DirectTCPIP(
            targetHost: destination.host,
            targetPort: destination.port,
            originatorAddress: originatorAddress
        )

        root.pipeline.handler(type: NIOSSHHandler.self).whenComplete { result in
            switch result {
            case let .failure(error):
                channelPromise.fail(error)
            case let .success(sshHandler):
                sshHandler.createChannel(
                    channelPromise,
                    channelType: .directTCPIP(directTCPIP)
                ) { childChannel, channelType in
                    guard case .directTCPIP = channelType else {
                        return childChannel.eventLoop.makeFailedFuture(
                            SSHTransportError.invalidChannelType
                        )
                    }

                    let (localGlue, sshGlue) = SSHForwardGlueHandler.matchedPair()
                    return localChannel.setOption(
                        ChannelOptions.autoRead,
                        value: false
                    ).and(
                        childChannel.setOption(
                            ChannelOptions.autoRead,
                            value: false
                        )
                    ).flatMap { _ in
                        childChannel.eventLoop.makeCompletedFuture {
                            let sshPipeline = childChannel.pipeline.syncOperations
                            try sshPipeline.addHandler(SSHChannelDataWrapper())
                            try sshPipeline.addHandler(sshGlue)
                            try localChannel.pipeline.syncOperations.addHandler(localGlue)
                        }
                    }
                }
            }
        }

        let deadline = NIODeadline.now() + directOpenTimeout
        let boundedOpen = Self.forwardChannelFuture(
            channelPromise.futureResult,
            localChannel: localChannel,
            root: root,
            deadline: deadline
        )

        return boundedOpen.flatMap { [weak self] sshChannel in
            guard let self,
                  sshChannel.isActive,
                  self.registerForward(
                    sshChannel,
                    identifier: identifier,
                    generation: currentGeneration
                  )
            else {
                sshChannel.close(promise: nil)
                return localChannel.eventLoop.makeFailedFuture(
                    SSHTransportError.connectionCancelled
                )
            }

            sshChannel.closeFuture.whenComplete { [weak self] _ in
                self?.unregisterForward(identifier: identifier)
            }
            localChannel.closeFuture.whenComplete { _ in
                sshChannel.close(promise: nil)
            }
            // Both channels use manual reads. Each read-complete schedules at
            // most one successor read while its peer remains writable, so the
            // normal NIO write watermark bounds queued signaling bytes.
            return localChannel.pipeline.handler(
                type: SSHForwardGlueHandler.self
            ).and(
                sshChannel.pipeline.handler(type: SSHForwardGlueHandler.self)
            ).map { localGlue, sshGlue in
                // Start only after the child is generation-registered. If the
                // accepted socket is not active until its initializer returns,
                // channelActive will perform the deferred first read.
                localGlue.startReading()
                sshGlue.startReading()
            }
        }
    }

    private func connectedTerminalChannel() throws -> Channel {
        try withLock {
            guard stateStorage == .connected, let terminalChannel else {
                throw SSHTransportError.notConnected
            }
            return terminalChannel
        }
    }

    private static func createChildChannel(
        on root: Channel,
        channelType: SSHChannelType,
        initializer: @escaping (Channel, SSHChannelType) -> EventLoopFuture<Void>
    ) -> EventLoopFuture<Channel> {
        root.pipeline.handler(type: NIOSSHHandler.self).flatMap { sshHandler in
            let promise = root.eventLoop.makePromise(of: Channel.self)
            sshHandler.createChannel(
                promise,
                channelType: channelType,
                initializer
            )
            return promise.futureResult
        }
    }

    /// Resolves a future once, no later than an absolute deadline. A late
    /// successful value can be disposed explicitly, which is essential for
    /// NIOSSH child channels because the underlying open request is not
    /// cancellable.
    private static func future<Value>(
        _ source: EventLoopFuture<Value>,
        boundedBy deadline: NIODeadline,
        timeoutError: Error,
        onTimeout: @escaping () -> Void = {},
        onLateSuccess: @escaping (Value) -> Void = { _ in }
    ) -> EventLoopFuture<Value> {
        let completion = SSHOneShotPromise<Value>(eventLoop: source.eventLoop)
        let timeoutTask = source.eventLoop.scheduleTask(deadline: deadline) {
            guard completion.complete(.failure(timeoutError)) else { return }
            onTimeout()
        }
        source.whenComplete { result in
            if completion.complete(result) {
                timeoutTask.cancel()
            } else if case let .success(value) = result {
                onLateSuccess(value)
            }
        }
        return completion.futureResult
    }

    /// Cancellation-aware async bridge for EventLoopFuture. NIO's native
    /// `get()` intentionally does not observe Swift task cancellation, so exec
    /// child creation needs an explicit race to prevent a cancelled late child
    /// from sending its command after a replacement operation has begun.
    private static func cancellableValue<Value: Sendable>(
        _ source: EventLoopFuture<Value>,
        boundedBy deadline: NIODeadline,
        timeoutError: Error,
        onTimeout: @escaping () -> Void = {},
        onCancel: @escaping () -> Bool = { true },
        onLateSuccess: @escaping (Value) -> Void = { _ in },
        checkCancellationAfterValue: Bool = true
    ) async throws -> Value {
        let completion = SSHOneShotPromise<Value>(eventLoop: source.eventLoop)
        let timeoutTask = source.eventLoop.scheduleTask(deadline: deadline) {
            guard completion.complete(.failure(timeoutError)) else { return }
            onTimeout()
        }
        source.whenComplete { result in
            if completion.complete(result) {
                timeoutTask.cancel()
            } else if case let .success(value) = result {
                onLateSuccess(value)
            }
        }

        let value = try await withTaskCancellationHandler {
            return try await completion.futureResult.get()
        } onCancel: {
            guard onCancel() else { return }
            guard completion.complete(.failure(CancellationError())) else { return }
            timeoutTask.cancel()
        }
        if checkCancellationAfterValue {
            try Task.checkCancellation()
        }
        return value
    }

    /// Races a direct child open against both the local originator lifetime and
    /// an absolute deadline. If the local socket vanishes while NIOSSH still
    /// has no child handle, the root is closed fail-closed so the pending open
    /// cannot live forever. Any late child is also closed defensively.
    private static func forwardChannelFuture(
        _ source: EventLoopFuture<Channel>,
        localChannel: Channel,
        root: Channel,
        deadline: NIODeadline
    ) -> EventLoopFuture<Channel> {
        let completion = SSHOneShotPromise<Channel>(eventLoop: source.eventLoop)
        let timeoutTask = source.eventLoop.scheduleTask(deadline: deadline) {
            guard completion.complete(
                .failure(SSHTransportError.forwardOpenTimedOut)
            ) else { return }
            root.close(promise: nil)
        }
        source.whenComplete { result in
            if completion.complete(result) {
                timeoutTask.cancel()
            } else if case let .success(channel) = result {
                channel.close(promise: nil)
            }
        }
        localChannel.closeFuture.whenComplete { _ in
            guard completion.complete(
                .failure(SSHTransportError.forwardOriginatorClosed)
            ) else { return }
            timeoutTask.cancel()
            // No child handle exists yet. Root close is the only supported
            // cancellation mechanism for the pending NIOSSH open request.
            root.close(promise: nil)
        }
        return completion.futureResult
    }

    private func connectedRootChannel() throws -> Channel {
        try connectedRootSnapshot().0
    }

    private func connectedRootSnapshot() throws -> (Channel, UInt64) {
        try withLock {
            guard stateStorage == .connected, let rootChannel else {
                throw SSHTransportError.notConnected
            }
            return (rootChannel, generation)
        }
    }

    private func registerRoot(_ channel: Channel, generation: UInt64) -> Bool {
        withLock {
            guard self.generation == generation, stateStorage == .connecting else {
                return false
            }
            rootChannel = channel
            return true
        }
    }

    private func registerTerminal(_ channel: Channel, generation: UInt64) -> Bool {
        withLock {
            guard self.generation == generation, stateStorage == .connecting else {
                return false
            }
            terminalChannel = channel
            return true
        }
    }

    private func registerExec(
        _ channel: Channel,
        identifier: UUID,
        generation: UInt64
    ) -> Bool {
        withLock {
            guard self.generation == generation,
                  stateStorage == .connected
            else {
                return false
            }
            execChannels[identifier] = channel
            return true
        }
    }

    private func unregisterExec(identifier: UUID) {
        _ = withLock { execChannels.removeValue(forKey: identifier) }
    }

    private func registerForward(
        _ channel: Channel,
        identifier: UUID,
        generation: UInt64
    ) -> Bool {
        withLock {
            guard self.generation == generation,
                  stateStorage == .connected
            else {
                return false
            }
            forwardChannels[identifier] = channel
            return true
        }
    }

    private func unregisterForward(identifier: UUID) {
        _ = withLock { forwardChannels.removeValue(forKey: identifier) }
    }

    private func emitOutput(_ chunk: SSHOutputChunk, generation: UInt64) {
        guard !chunk.data.isEmpty else { return }
        let outcome: (scheduleDrain: Bool, overflowed: Bool) = withLock {
            guard self.generation == generation,
                  stateStorage == .connecting || stateStorage == .connected
            else {
                return (false, false)
            }
            guard pendingOutputBytes <= maximumPendingOutputBytes - chunk.data.count else {
                return (false, true)
            }

            if let lastIndex = pendingOutput.indices.last,
               pendingOutput[lastIndex].stream == chunk.stream {
                // Mutate through Array's modify accessor so uniquely owned
                // Data storage can grow in place instead of copying the whole
                // coalesced prefix for every packet.
                pendingOutput[lastIndex].data.append(chunk.data)
            } else {
                guard pendingOutputChunkCount < maximumPendingOutputChunks else {
                    return (false, true)
                }
                pendingOutput.append(chunk)
                pendingOutputChunkCount += 1
            }
            pendingOutputBytes += chunk.data.count
            guard !isOutputDrainScheduled else {
                return (false, false)
            }
            isOutputDrainScheduled = true
            return (true, false)
        }

        if outcome.overflowed {
            unexpectedConnectionEnd(
                error: SSHTransportError.terminalOutputBacklogExceeded,
                generation: generation
            )
            return
        }
        guard outcome.scheduleDrain else { return }
        scheduleOutputDrain(generation: generation)
    }

    private func scheduleOutputDrain(generation: UInt64) {
        callbackQueue.async { [weak self] in
            guard let self else { return }
            let batch: [SSHOutputChunk] = self.withLock {
                guard self.generation == generation,
                      self.stateStorage == .connecting
                        || self.stateStorage == .connected
                else {
                    self.resetPendingOutputLocked()
                    return []
                }
                let batch = self.pendingOutput
                self.pendingOutput.removeAll(keepingCapacity: true)
                return batch
            }
            guard !batch.isEmpty else { return }

            for chunk in batch {
                self.callbacks.onOutput(chunk)
            }

            let shouldContinue: Bool = self.withLock {
                let deliveredBytes = batch.reduce(into: 0) {
                    $0 += $1.data.count
                }
                self.pendingOutputBytes = max(
                    0,
                    self.pendingOutputBytes - deliveredBytes
                )
                self.pendingOutputChunkCount = max(
                    0,
                    self.pendingOutputChunkCount - batch.count
                )
                guard self.generation == generation,
                      self.stateStorage == .connecting
                        || self.stateStorage == .connected,
                      !self.pendingOutput.isEmpty
                else {
                    self.isOutputDrainScheduled = false
                    if self.generation != generation
                        || (self.stateStorage != .connecting
                            && self.stateStorage != .connected) {
                        self.resetPendingOutputLocked()
                    }
                    return false
                }
                return true
            }
            if shouldContinue {
                self.scheduleOutputDrain(generation: generation)
            }
        }
    }

    private func emitState(_ state: SSHTransportState) {
        callbackQueue.async { [callbacks] in
            callbacks.onStateChange(state)
        }
    }

    private func unexpectedConnectionEnd(error: Error, generation: UInt64) {
        let shouldTearDown = withLock {
            guard self.generation == generation,
                  stateStorage == .connecting || stateStorage == .connected,
                  !isFailureTeardownScheduled
            else {
                return false
            }
            isFailureTeardownScheduled = true
            return true
        }
        guard shouldTearDown else { return }
        Task { [weak self] in
            await self?.failAndTearDown(error: error, generation: generation)
        }
    }

    private func failAndTearDown(error: Error, generation: UInt64) async {
        let outcome: (didFail: Bool, resources: TransportResources?) = withLock {
            guard self.generation == generation,
                  stateStorage == .connecting || stateStorage == .connected
            else {
                return (false, nil)
            }
            stateStorage = .failed(error.localizedDescription)
            return (true, takeResourcesLocked())
        }
        guard outcome.didFail else { return }
        emitState(.failed(error.localizedDescription))
        if let resources = outcome.resources {
            await Self.closeResources(resources)
        }
    }

    private func takeResourcesLocked() -> TransportResources? {
        resetPendingOutputLocked()
        guard rootChannel != nil
                || terminalChannel != nil
                || !execChannels.isEmpty
                || !forwardChannels.isEmpty
        else {
            return nil
        }
        let resources = TransportResources(
            rootChannel: rootChannel,
            terminalChannel: terminalChannel,
            execChannels: Array(execChannels.values),
            forwardChannels: Array(forwardChannels.values)
        )
        rootChannel = nil
        terminalChannel = nil
        execChannels.removeAll(keepingCapacity: false)
        forwardChannels.removeAll(keepingCapacity: false)
        return resources
    }

    private static func closeResources(_ resources: TransportResources) async {
        for channel in resources.execChannels {
            try? await channel.close()
        }
        for channel in resources.forwardChannels {
            try? await channel.close()
        }
        try? await resources.terminalChannel?.close()
        try? await resources.rootChannel?.close()
    }

    private func withLock<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }

    private func resetPendingOutputLocked() {
        pendingOutput.removeAll(keepingCapacity: false)
        pendingOutputBytes = 0
        pendingOutputChunkCount = 0
        isOutputDrainScheduled = false
    }

    private static func isValidTerminalType(_ value: String) -> Bool {
        guard !value.isEmpty, value.utf8.count <= 64 else { return false }
        return value.unicodeScalars.allSatisfy {
            CharacterSet(
                charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._+-"
            ).contains($0)
        }
    }
}

enum SSHTransportError: Error, Equatable, LocalizedError {
    case invalidIdentity
    case invalidConfiguration
    case invalidTerminalSize
    case invalidTerminalType
    case invalidExecLimits
    case invalidCommand
    case alreadyConnected
    case notConnected
    case connectionCancelled
    case connectionClosed
    case connectionTimedOut
    case sessionClosed
    case invalidChannelType
    case inputChunkTooLarge
    case terminalOutputBacklogExceeded
    case ptyRequestRejected
    case shellRequestRejected
    case execRequestRejected
    case execOutputLimitExceeded
    case execTimedOut
    case execExitedBySignal(String)
    case execClosedWithoutStatus
    case commandFailed(exitStatus: Int)
    case unexpectedChannelData
    case forwardRequiresSharedEventLoop
    case missingOriginatorAddress
    case forwardOriginatorClosed
    case forwardOpenTimedOut

    var errorDescription: String? {
        switch self {
        case .invalidIdentity:
            return "The saved Ed25519 SSH identity is invalid."
        case .invalidConfiguration:
            return "The SSH connection configuration is invalid."
        case .invalidTerminalSize:
            return "The SSH terminal dimensions are invalid."
        case .invalidTerminalType:
            return "The SSH terminal type is invalid."
        case .invalidExecLimits:
            return "The SSH command limits are invalid."
        case .invalidCommand:
            return "The SSH command is empty or too large."
        case .alreadyConnected:
            return "The SSH transport is already active."
        case .notConnected:
            return "The SSH transport is not connected."
        case .connectionCancelled:
            return "The SSH connection was cancelled."
        case .connectionClosed:
            return "The SSH connection closed."
        case .connectionTimedOut:
            return "SSH authentication or terminal setup timed out."
        case .sessionClosed:
            return "The remote SSH terminal session closed."
        case .invalidChannelType:
            return "The SSH server opened an unexpected channel type."
        case .inputChunkTooLarge:
            return "The SSH input chunk is too large."
        case .terminalOutputBacklogExceeded:
            return "The SSH terminal output exceeded the local delivery backlog limit."
        case .ptyRequestRejected:
            return "The SSH server rejected the pseudo-terminal request."
        case .shellRequestRejected:
            return "The SSH server rejected the interactive shell request."
        case .execRequestRejected:
            return "The SSH server rejected the command request."
        case .execOutputLimitExceeded:
            return "The SSH command exceeded its output limit."
        case .execTimedOut:
            return "The SSH command timed out."
        case let .execExitedBySignal(signal):
            return "The SSH command exited after signal \(signal)."
        case .execClosedWithoutStatus:
            return "The SSH command channel closed without an exit status."
        case let .commandFailed(exitStatus):
            return "The SSH command exited with status \(exitStatus)."
        case .unexpectedChannelData:
            return "The SSH channel returned unsupported data."
        case .forwardRequiresSharedEventLoop:
            return "The preview forward must share the SSH transport event loop."
        case .missingOriginatorAddress:
            return "The preview forward has no loopback originator address."
        case .forwardOriginatorClosed:
            return "The local preview socket closed before its SSH forward opened."
        case .forwardOpenTimedOut:
            return "The SSH preview forward timed out while opening."
        }
    }
}

private struct TransportResources {
    let rootChannel: Channel?
    let terminalChannel: Channel?
    let execChannels: [Channel]
    let forwardChannels: [Channel]
}

/// A lock-protected single-completion EventLoop promise. Deadline callbacks,
/// channel futures, and local-close futures may race on different executors.
private final class SSHOneShotPromise<Value>: @unchecked Sendable {
    private let promise: EventLoopPromise<Value>
    private let lock = NSLock()
    private var isComplete = false

    init(eventLoop: EventLoop) {
        promise = eventLoop.makePromise(of: Value.self)
    }

    var futureResult: EventLoopFuture<Value> {
        promise.futureResult
    }

    @discardableResult
    func complete(_ result: Result<Value, Error>) -> Bool {
        lock.lock()
        guard !isComplete else {
            lock.unlock()
            return false
        }
        isComplete = true
        lock.unlock()
        promise.completeWith(result)
        return true
    }
}

/// Completes the PTY setup future exactly once even when authentication or
/// child-channel creation fails before the PTY handler is installed.
private final class SSHOneShotVoidPromise: @unchecked Sendable {
    private let promise: EventLoopPromise<Void>
    private let lock = NSLock()
    private var isComplete = false

    init(eventLoop: EventLoop) {
        promise = eventLoop.makePromise(of: Void.self)
    }

    var futureResult: EventLoopFuture<Void> {
        promise.futureResult
    }

    func succeed() {
        guard claim() else { return }
        promise.succeed(())
    }

    func fail(_ error: Error) {
        guard claim() else { return }
        promise.fail(error)
    }

    private func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !isComplete else { return false }
        isComplete = true
        return true
    }
}

/// Coordinates Swift task cancellation with an SSH exec child before that
/// child is visible to `execute()`. Registration happens in the NIOSSH child
/// initializer, so cancellation can close the channel and prevent ExecRequest
/// even while createChannel's outer future is still unresolved.
private final class SSHExecCancellationGate: @unchecked Sendable {
    private enum State {
        case pending
        case requestStarted
        case cancelledBeforeRequest
        case cancelledAfterRequest
    }

    private let lock = NSLock()
    private var state: State = .pending
    private var channel: Channel?

    func register(_ channel: Channel) -> Bool {
        let didRegister: Bool
        lock.lock()
        switch state {
        case .pending:
            self.channel = channel
            didRegister = true
        case .requestStarted, .cancelledBeforeRequest, .cancelledAfterRequest:
            didRegister = false
        }
        lock.unlock()

        if !didRegister {
            channel.close(promise: nil)
        }
        return didRegister
    }

    func beginRequest() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard case .pending = state else { return false }
        state = .requestStarted
        return true
    }

    /// Returns `true` when the waiting async bridge may fail immediately.
    /// Once ExecRequest has started, cancellation must wait for the bounded
    /// command result so a mutating command cannot outlive its caller and race
    /// a replacement operation on the same SSH transport.
    func requestTaskCancellation() -> Bool {
        let channelToClose: Channel?
        let mayFailImmediately: Bool
        lock.lock()
        switch state {
        case .pending:
            state = .cancelledBeforeRequest
            channelToClose = channel
            mayFailImmediately = true
        case .requestStarted:
            state = .cancelledAfterRequest
            channelToClose = nil
            mayFailImmediately = false
        case .cancelledBeforeRequest:
            channelToClose = channel
            mayFailImmediately = true
        case .cancelledAfterRequest:
            channelToClose = nil
            mayFailImmediately = false
        }
        lock.unlock()
        channelToClose?.close(promise: nil)
        return mayFailImmediately
    }

    /// Unconditionally closes the child while unwinding an error or timeout.
    func forceCancel() {
        let channelToClose: Channel?
        lock.lock()
        switch state {
        case .pending, .cancelledBeforeRequest:
            state = .cancelledBeforeRequest
        case .requestStarted, .cancelledAfterRequest:
            state = .cancelledAfterRequest
        }
        channelToClose = channel
        lock.unlock()
        channelToClose?.close(promise: nil)
    }

    func clear(_ channel: Channel) {
        lock.lock()
        if self.channel === channel {
            self.channel = nil
        }
        lock.unlock()
    }
}

private final class SequentialSSHAuthenticationDelegate: NIOSSHClientUserAuthenticationDelegate, @unchecked Sendable {
    private let username: String
    private let lock = NSLock()
    private var remaining: [SSHAuthenticationCredential]

    init(username: String, credentials: [SSHAuthenticationCredential]) {
        self.username = username
        remaining = credentials
    }

    func nextAuthenticationType(
        availableMethods: NIOSSHAvailableUserAuthenticationMethods,
        nextChallengePromise: EventLoopPromise<NIOSSHUserAuthenticationOffer?>
    ) {
        lock.lock()
        let index = remaining.firstIndex { credential in
            switch credential {
            case .ed25519:
                return availableMethods.contains(.publicKey)
            case .password:
                return availableMethods.contains(.password)
            }
        }
        let credential = index.map { remaining.remove(at: $0) }
        lock.unlock()

        guard let credential else {
            nextChallengePromise.succeed(nil)
            return
        }

        let offer: NIOSSHUserAuthenticationOffer.Offer
        switch credential {
        case let .ed25519(identity):
            offer = .privateKey(.init(privateKey: identity.nioPrivateKey))
        case let .password(password):
            offer = .password(.init(password: password))
        }
        nextChallengePromise.succeed(
            .init(username: username, serviceName: "ssh-connection", offer: offer)
        )
    }
}

private final class SSHRootLifecycleHandler: ChannelInboundHandler {
    typealias InboundIn = Any

    private let onError: (Error) -> Void
    private let onInactive: () -> Void

    init(onError: @escaping (Error) -> Void, onInactive: @escaping () -> Void) {
        self.onError = onError
        self.onInactive = onInactive
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        onError(error)
        context.close(promise: nil)
    }

    func channelInactive(context: ChannelHandlerContext) {
        onInactive()
        context.fireChannelInactive()
    }
}

private final class SSHPTYChannelHandler: ChannelInboundHandler {
    typealias InboundIn = SSHChannelData

    private enum SetupState {
        case waitingForPTYReply
        case waitingForShellReply
        case ready
        case closed
    }

    private let initialSize: SSHTerminalSize
    private let terminalType: String
    private let readyCompletion: SSHOneShotVoidPromise
    private let onOutput: (SSHOutputChunk) -> Void
    private let onClosed: (Error?) -> Void
    private var setupState: SetupState = .waitingForPTYReply
    private var didReportClosure = false

    init(
        initialSize: SSHTerminalSize,
        terminalType: String,
        readyCompletion: SSHOneShotVoidPromise,
        onOutput: @escaping (SSHOutputChunk) -> Void,
        onClosed: @escaping (Error?) -> Void
    ) {
        self.initialSize = initialSize
        self.terminalType = terminalType
        self.readyCompletion = readyCompletion
        self.onOutput = onOutput
        self.onClosed = onClosed
    }

    func channelActive(context: ChannelHandlerContext) {
        let request = SSHChannelRequestEvent.PseudoTerminalRequest(
            wantReply: true,
            term: terminalType,
            terminalCharacterWidth: initialSize.columns,
            terminalRowHeight: initialSize.rows,
            terminalPixelWidth: initialSize.pixelWidth,
            terminalPixelHeight: initialSize.pixelHeight,
            terminalModes: .init([:])
        )
        let promise = context.eventLoop.makePromise(of: Void.self)
        promise.futureResult.whenFailure { [weak self, weak context] error in
            guard let self, let context else { return }
            self.failSetup(error, context: context)
        }
        context.triggerUserOutboundEvent(request, promise: promise)
        context.fireChannelActive()
    }

    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        switch event {
        case is ChannelSuccessEvent:
            switch setupState {
            case .waitingForPTYReply:
                setupState = .waitingForShellReply
                let request = SSHChannelRequestEvent.ShellRequest(wantReply: true)
                let promise = context.eventLoop.makePromise(of: Void.self)
                promise.futureResult.whenFailure { [weak self, weak context] error in
                    guard let self, let context else { return }
                    self.failSetup(error, context: context)
                }
                context.triggerUserOutboundEvent(request, promise: promise)
            case .waitingForShellReply:
                setupState = .ready
                readyCompletion.succeed()
            case .ready, .closed:
                context.fireUserInboundEventTriggered(event)
            }

        case is ChannelFailureEvent:
            let error: SSHTransportError
            switch setupState {
            case .waitingForPTYReply:
                error = .ptyRequestRejected
            case .waitingForShellReply:
                error = .shellRequestRejected
            case .ready, .closed:
                context.fireUserInboundEventTriggered(event)
                return
            }
            failSetup(error, context: context)

        case let exit as SSHChannelRequestEvent.ExitSignal:
            reportClosure(SSHTransportError.execExitedBySignal(exit.signalName))
            context.close(promise: nil)

        case is SSHChannelRequestEvent.ExitStatus:
            reportClosure(nil)
            context.close(promise: nil)

        default:
            context.fireUserInboundEventTriggered(event)
        }
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let channelData = unwrapInboundIn(data)
        guard case let .byteBuffer(buffer) = channelData.data else {
            failSetup(SSHTransportError.unexpectedChannelData, context: context)
            return
        }
        let stream: SSHOutputStream
        switch channelData.type {
        case .channel:
            stream = .standardOutput
        case .stdErr:
            stream = .standardError
        default:
            failSetup(SSHTransportError.unexpectedChannelData, context: context)
            return
        }
        if buffer.readableBytes > 0 {
            onOutput(SSHOutputChunk(stream: stream, data: Data(buffer.readableBytesView)))
        }
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        failSetup(error, context: context)
    }

    func channelInactive(context: ChannelHandlerContext) {
        readyCompletion.fail(SSHTransportError.sessionClosed)
        setupState = .closed
        reportClosure(nil)
        context.fireChannelInactive()
    }

    private func failSetup(_ error: Error, context: ChannelHandlerContext) {
        readyCompletion.fail(error)
        setupState = .closed
        reportClosure(error)
        context.close(promise: nil)
    }

    private func reportClosure(_ error: Error?) {
        guard !didReportClosure else { return }
        didReportClosure = true
        onClosed(error)
    }
}

private final class SSHExecChannelHandler: ChannelInboundHandler {
    typealias InboundIn = SSHChannelData

    private let command: String
    private let maximumOutputBytes: Int
    private let cancellationGate: SSHExecCancellationGate
    private var resultPromise: EventLoopPromise<SSHExecResult>?
    private var standardOutput = Data()
    private var standardError = Data()
    private var requestWasAccepted = false

    init(
        command: String,
        maximumOutputBytes: Int,
        resultPromise: EventLoopPromise<SSHExecResult>,
        cancellationGate: SSHExecCancellationGate
    ) {
        self.command = command
        self.maximumOutputBytes = maximumOutputBytes
        self.resultPromise = resultPromise
        self.cancellationGate = cancellationGate
    }

    func channelActive(context: ChannelHandlerContext) {
        guard cancellationGate.beginRequest() else {
            finish(.failure(CancellationError()), context: context)
            return
        }
        let request = SSHChannelRequestEvent.ExecRequest(command: command, wantReply: true)
        let promise = context.eventLoop.makePromise(of: Void.self)
        promise.futureResult.whenFailure { [weak self, weak context] error in
            guard let self, let context else { return }
            self.finish(.failure(error), context: context)
        }
        context.triggerUserOutboundEvent(request, promise: promise)
        context.fireChannelActive()
    }

    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        switch event {
        case is ChannelSuccessEvent:
            requestWasAccepted = true
        case is ChannelFailureEvent where !requestWasAccepted:
            finish(.failure(SSHTransportError.execRequestRejected), context: context)
        case let status as SSHChannelRequestEvent.ExitStatus:
            finish(
                .success(
                    SSHExecResult(
                        exitStatus: status.exitStatus,
                        standardOutput: standardOutput,
                        standardError: standardError
                    )
                ),
                context: context
            )
        case let signal as SSHChannelRequestEvent.ExitSignal:
            finish(
                .failure(SSHTransportError.execExitedBySignal(signal.signalName)),
                context: context
            )
        default:
            context.fireUserInboundEventTriggered(event)
        }
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let channelData = unwrapInboundIn(data)
        guard case let .byteBuffer(buffer) = channelData.data else {
            finish(.failure(SSHTransportError.unexpectedChannelData), context: context)
            return
        }
        let incoming = Data(buffer.readableBytesView)
        guard standardOutput.count + standardError.count + incoming.count
            <= maximumOutputBytes
        else {
            finish(.failure(SSHTransportError.execOutputLimitExceeded), context: context)
            return
        }

        switch channelData.type {
        case .channel:
            standardOutput.append(incoming)
        case .stdErr:
            standardError.append(incoming)
        default:
            finish(.failure(SSHTransportError.unexpectedChannelData), context: context)
        }
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        finish(.failure(error), context: context)
    }

    func channelInactive(context: ChannelHandlerContext) {
        if resultPromise != nil {
            finish(.failure(SSHTransportError.execClosedWithoutStatus), context: context)
        }
        context.fireChannelInactive()
    }

    private func finish(
        _ result: Result<SSHExecResult, Error>,
        context: ChannelHandlerContext
    ) {
        guard let resultPromise else { return }
        self.resultPromise = nil
        resultPromise.completeWith(result)
        context.close(promise: nil)
    }
}

/// Converts direct-tcpip `SSHChannelData` to the raw `ByteBuffer` used by the
/// loopback socket, and wraps writes in the other direction.
private final class SSHChannelDataWrapper: ChannelDuplexHandler {
    typealias InboundIn = SSHChannelData
    typealias InboundOut = ByteBuffer
    typealias OutboundIn = ByteBuffer
    typealias OutboundOut = SSHChannelData

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let channelData = unwrapInboundIn(data)
        guard channelData.type == .channel,
              case let .byteBuffer(buffer) = channelData.data
        else {
            context.fireErrorCaught(SSHTransportError.unexpectedChannelData)
            return
        }
        context.fireChannelRead(wrapInboundOut(buffer))
    }

    func write(
        context: ChannelHandlerContext,
        data: NIOAny,
        promise: EventLoopPromise<Void>?
    ) {
        let buffer = unwrapOutboundIn(data)
        context.write(
            wrapOutboundOut(
                SSHChannelData(type: .channel, data: .byteBuffer(buffer))
            ),
            promise: promise
        )
    }
}

/// A backpressure-aware pair used only on the transport's single event loop.
private final class SSHForwardGlueHandler: ChannelDuplexHandler {
    typealias InboundIn = NIOAny
    typealias OutboundIn = NIOAny
    typealias OutboundOut = NIOAny

    private weak var partner: SSHForwardGlueHandler?
    private var context: ChannelHandlerContext?
    private var pendingRead = false
    private var isReadingStarted = false

    private init() {}

    static func matchedPair() -> (SSHForwardGlueHandler, SSHForwardGlueHandler) {
        let first = SSHForwardGlueHandler()
        let second = SSHForwardGlueHandler()
        first.partner = second
        second.partner = first
        return (first, second)
    }

    func handlerAdded(context: ChannelHandlerContext) {
        self.context = context
        if context.channel.isWritable {
            partner?.partnerBecameWritable()
        }
        if isReadingStarted, context.channel.isActive {
            context.channel.read()
        }
    }

    func channelActive(context: ChannelHandlerContext) {
        if isReadingStarted {
            context.channel.read()
        }
        context.fireChannelActive()
    }

    func handlerRemoved(context: ChannelHandlerContext) {
        self.context = nil
        partner = nil
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        partner?.context?.write(data, promise: nil)
    }

    func channelReadComplete(context: ChannelHandlerContext) {
        partner?.context?.flush()
        read(context: context)
    }

    func channelInactive(context: ChannelHandlerContext) {
        partner?.context?.close(promise: nil)
    }

    func userInboundEventTriggered(context: ChannelHandlerContext, event: Any) {
        if let channelEvent = event as? ChannelEvent, channelEvent == .inputClosed {
            partner?.context?.close(mode: .output, promise: nil)
        } else {
            context.fireUserInboundEventTriggered(event)
        }
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        partner?.context?.close(promise: nil)
        context.close(promise: nil)
    }

    func channelWritabilityChanged(context: ChannelHandlerContext) {
        if context.channel.isWritable {
            partner?.partnerBecameWritable()
        }
    }

    func read(context: ChannelHandlerContext) {
        guard isReadingStarted else { return }
        if partner?.context?.channel.isWritable == true {
            context.read()
        } else {
            pendingRead = true
        }
    }

    func startReading() {
        guard !isReadingStarted else { return }
        isReadingStarted = true
        if let context, context.channel.isActive {
            context.channel.read()
        }
    }

    private func partnerBecameWritable() {
        if pendingRead {
            pendingRead = false
            context?.read()
        }
    }
}
