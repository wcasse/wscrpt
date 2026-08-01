import Foundation
import NIOCore
import NIOPosix

struct SSHForwardDestination: Equatable, Sendable {
    let host: String
    let port: Int

    init(host: String, port: Int) throws {
        guard host == "127.0.0.1", (1 ... 65_535).contains(port) else {
            throw SSHLocalForwardError.nonLoopbackDestination
        }
        self.host = host
        self.port = port
    }
}

/// Opens one SSH `direct-tcpip` child for one accepted local TCP socket.
///
/// The returned child channel must be on the supplied originator's event loop
/// and expose `ByteBuffer` inbound and outbound at its pipeline tail (the SSH
/// transport normally installs its `SSHChannelData` codec while creating the
/// child). `SSHLocalForward` then owns the byte-for-byte bridge and lifecycle.
protocol SSHDirectTCPIPOpening: Sendable {
    func eventLoopGroupForDirectTCPIP() throws -> EventLoopGroup

    func openDirectTCPIP(
        destination: SSHForwardDestination,
        originatorAddress: SocketAddress
    ) -> EventLoopFuture<Channel>
}

/// Legacy-compatible connector seam for a transport that already owns the
/// channel bridge. It must create exactly one direct-tcpip SSH child for the
/// supplied accepted socket and close the peer if either side closes.
typealias SSHDirectTCPIPConnector = @Sendable (
    _ localChannel: Channel,
    _ destination: SSHForwardDestination
) -> EventLoopFuture<Void>

/// A numeric-loopback-only local TCP listener for preview HTTP/WebSocket
/// signaling. WebRTC media remains direct between the browser and iPad.
final class SSHLocalForward: @unchecked Sendable {
    static let bindHost = "127.0.0.1"
    static let maximumAcceptedConnections = 16

    private enum ListenerState {
        case idle
        case starting(closeRequested: Bool)
        case listening(Channel)
    }

    private let eventLoopGroup: EventLoopGroup
    private let destination: SSHForwardDestination
    private let connector: SSHDirectTCPIPConnector
    private let acceptedChannels = SSHForwardChannelRegistry()
    private let lock = NSLock()
    private var listenerState: ListenerState = .idle

    init(
        eventLoopGroup: EventLoopGroup,
        destination: SSHForwardDestination,
        connector: @escaping SSHDirectTCPIPConnector
    ) {
        self.eventLoopGroup = eventLoopGroup
        self.destination = destination
        self.connector = connector
    }

    /// Convenience path for transports that expose an unbridged, ByteBuffer-
    /// normalized direct-tcpip child channel.
    convenience init(
        opener: any SSHDirectTCPIPOpening,
        destination: SSHForwardDestination
    ) throws {
        let eventLoopGroup = try opener.eventLoopGroupForDirectTCPIP()
        self.init(
            eventLoopGroup: eventLoopGroup,
            destination: destination
        ) { localChannel, destination in
            guard let originator = localChannel.remoteAddress else {
                return localChannel.eventLoop.makeFailedFuture(
                    SSHLocalForwardError.invalidOriginatorAddress
                )
            }
            return opener.openDirectTCPIP(
                destination: destination,
                originatorAddress: originator
            ).flatMap { sshChannel in
                Self.installByteBufferBridge(
                    localChannel: localChannel,
                    sshChannel: sshChannel
                )
            }
        }
    }

    /// Binds `127.0.0.1:0` and returns only after the kernel-assigned numeric
    /// port is listening. This ordering is the credential-minting gate used by
    /// `PreviewCoordinator`.
    func start() async throws -> UInt16 {
        try withLock {
            guard case .idle = listenerState else {
                throw SSHLocalForwardError.alreadyStarted
            }
            listenerState = .starting(closeRequested: false)
        }

        let destination = destination
        let connector = connector
        let acceptedChannels = acceptedChannels
        let channel: Channel
        do {
            channel = try await ServerBootstrap(group: eventLoopGroup)
                .serverChannelOption(
                    ChannelOptions.socketOption(.so_reuseaddr),
                    value: 1
                )
                .childChannelOption(
                    ChannelOptions.allowRemoteHalfClosure,
                    value: true
                )
                .childChannelOption(
                    ChannelOptions.autoRead,
                    value: false
                )
                .childChannelOption(
                    ChannelOptions.socketOption(.tcp_nodelay),
                    value: 1
                )
                .childChannelInitializer { localChannel in
                    guard Self.isExactLoopbackPeer(
                        localChannel.remoteAddress
                    ) else {
                        return localChannel.eventLoop.makeFailedFuture(
                            SSHLocalForwardError.nonLoopbackPeer
                        )
                    }

                    guard acceptedChannels.insert(localChannel) else {
                        return localChannel.eventLoop.makeFailedFuture(
                            SSHLocalForwardError.connectionLimitExceeded
                        )
                    }
                    localChannel.closeFuture.whenComplete { _ in
                        acceptedChannels.remove(localChannel)
                    }
                    return connector(localChannel, destination).flatMapError {
                        error in
                        acceptedChannels.remove(localChannel)
                        // A failed/timed-out SSH child open must not leave the
                        // accepted browser socket alive while a bootstrap
                        // initializer future fails in the background.
                        localChannel.close(promise: nil)
                        return localChannel.eventLoop.makeFailedFuture(error)
                    }
                }
                .bind(host: Self.bindHost, port: 0)
                .get()
        } catch {
            resetAfterFailedStart()
            throw error
        }

        guard let address = channel.localAddress,
              Self.isExactLoopbackListener(address),
              let port = address.port,
              let localPort = UInt16(exactly: port),
              localPort > 0
        else {
            resetAfterFailedStart()
            try? await channel.close()
            await acceptedChannels.closeAll()
            throw SSHLocalForwardError.invalidListenerAddress
        }

        let shouldClose = withLock {
            switch listenerState {
            case .starting(closeRequested: false):
                listenerState = .listening(channel)
                return false
            case .starting(closeRequested: true):
                listenerState = .idle
                return true
            case .idle, .listening:
                return true
            }
        }

        if shouldClose {
            try? await channel.close()
            await acceptedChannels.closeAll()
            throw SSHLocalForwardError.startCancelled
        }
        return localPort
    }

    func close() async {
        let listener: Channel? = withLock {
            switch listenerState {
            case .idle:
                return nil
            case .starting:
                listenerState = .starting(closeRequested: true)
                return nil
            case let .listening(channel):
                listenerState = .idle
                return channel
            }
        }

        // Stop new accepts first, then close every accepted HTTP/WebSocket
        // socket; the connector/relay closes its matching SSH child.
        try? await listener?.close()
        await acceptedChannels.closeAll()
    }

    static func isExactLoopbackListener(_ address: SocketAddress) -> Bool {
        address.ipAddress == bindHost && (address.port ?? 0) > 0
    }

    static func isExactLoopbackPeer(_ address: SocketAddress?) -> Bool {
        address?.ipAddress == bindHost
    }

    private func resetAfterFailedStart() {
        withLock {
            listenerState = .idle
        }
    }

    private func withLock<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try body()
    }

    private static func installByteBufferBridge(
        localChannel: Channel,
        sshChannel: Channel
    ) -> EventLoopFuture<Void> {
        guard ObjectIdentifier(localChannel.eventLoop)
                == ObjectIdentifier(sshChannel.eventLoop)
        else {
            return localChannel.eventLoop.makeFailedFuture(
                SSHLocalForwardError.invalidForwardEventLoop
            )
        }

        return localChannel.setOption(
            ChannelOptions.autoRead,
            value: false
        ).and(
            sshChannel.setOption(ChannelOptions.autoRead, value: false)
        ).flatMap { _ in
            localChannel.eventLoop.makeCompletedFuture {
                let (localRelay, sshRelay) = SSHForwardRelayHandler.matchedPair()
                try localChannel.pipeline.syncOperations.addHandler(localRelay)
                try sshChannel.pipeline.syncOperations.addHandler(sshRelay)
                localRelay.startReading()
                sshRelay.startReading()
            }
        }
    }
}

private final class SSHForwardChannelRegistry: @unchecked Sendable {
    private let lock = NSLock()
    private var channels: [ObjectIdentifier: Channel] = [:]

    @discardableResult
    func insert(_ channel: Channel) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard channels.count < SSHLocalForward.maximumAcceptedConnections else {
            return false
        }
        channels[ObjectIdentifier(channel)] = channel
        return true
    }

    func remove(_ channel: Channel) {
        lock.lock()
        channels.removeValue(forKey: ObjectIdentifier(channel))
        lock.unlock()
    }

    func closeAll() async {
        let snapshot = takeAll()

        for channel in snapshot {
            try? await channel.close()
        }
    }

    private func takeAll() -> [Channel] {
        lock.lock()
        defer { lock.unlock() }
        let snapshot = Array(channels.values)
        channels.removeAll(keepingCapacity: false)
        return snapshot
    }
}

/// Same-loop, backpressure-aware, half-close-preserving ByteBuffer relay. The
/// two handlers remove their cross-reference when either pipeline removes one,
/// avoiding a retained local/SSH channel pair after detach.
private final class SSHForwardRelayHandler: ChannelDuplexHandler {
    typealias InboundIn = ByteBuffer
    typealias OutboundIn = ByteBuffer
    typealias OutboundOut = ByteBuffer

    private var partner: SSHForwardRelayHandler?
    private var context: ChannelHandlerContext?
    private var pendingRead = false
    private var isReadingStarted = false

    private init() {}

    static func matchedPair()
        -> (SSHForwardRelayHandler, SSHForwardRelayHandler)
    {
        let first = SSHForwardRelayHandler()
        let second = SSHForwardRelayHandler()
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
        let oldPartner = partner
        partner = nil
        oldPartner?.partner = nil
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        partner?.partnerWrite(unwrapInboundIn(data))
    }

    func channelReadComplete(context: ChannelHandlerContext) {
        partner?.context?.flush()
        read(context: context)
    }

    func channelInactive(context: ChannelHandlerContext) {
        partner?.context?.close(promise: nil)
        context.fireChannelInactive()
    }

    func userInboundEventTriggered(
        context: ChannelHandlerContext,
        event: Any
    ) {
        if let channelEvent = event as? ChannelEvent,
           case .inputClosed = channelEvent {
            partner?.context?.close(mode: .output, promise: nil)
        }
        context.fireUserInboundEventTriggered(event)
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        partner?.context?.close(promise: nil)
        context.close(promise: nil)
    }

    func channelWritabilityChanged(context: ChannelHandlerContext) {
        if context.channel.isWritable {
            partner?.partnerBecameWritable()
        }
        context.fireChannelWritabilityChanged()
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

    private func partnerWrite(_ buffer: ByteBuffer) {
        guard let context else { return }
        context.write(wrapOutboundOut(buffer), promise: nil)
    }

    private func partnerBecameWritable() {
        guard pendingRead else { return }
        pendingRead = false
        context?.read()
    }
}

enum SSHLocalForwardError: Error, Equatable, LocalizedError {
    case nonLoopbackDestination
    case nonLoopbackPeer
    case invalidOriginatorAddress
    case invalidListenerAddress
    case invalidForwardEventLoop
    case connectionLimitExceeded
    case alreadyStarted
    case startCancelled

    var errorDescription: String? {
        switch self {
        case .nonLoopbackDestination:
            return "Preview forwarding is restricted to remote 127.0.0.1."
        case .nonLoopbackPeer:
            return "A non-loopback client attempted to use the preview tunnel."
        case .invalidOriginatorAddress:
            return "The local preview connection has no numeric origin address."
        case .invalidListenerAddress:
            return "The preview tunnel did not bind exact local loopback."
        case .invalidForwardEventLoop:
            return "The preview tunnel and SSH child must share one event loop."
        case .connectionLimitExceeded:
            return "The preview tunnel rejected excess concurrent loopback connections."
        case .alreadyStarted:
            return "The preview tunnel is already running."
        case .startCancelled:
            return "The preview tunnel was closed while it was starting."
        }
    }
}
