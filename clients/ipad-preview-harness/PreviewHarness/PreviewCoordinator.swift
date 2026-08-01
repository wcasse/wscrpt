import Combine
import Foundation
import NIOCore

/// One command sent through an authenticated SSH exec channel.
///
/// Every dynamic value is encoded as a distinct POSIX shell word. The only
/// expansion left in the outer command is the remote account's `$SHELL`; a
/// workspace beginning with `~/` is expanded through `$HOME` by construction,
/// never by interpolating untrusted text into shell syntax.
struct PreviewRemoteCommand: Equatable, Sendable {
    static let maximumWordBytes = 16_384
    static let maximumCommandBytes = 64 * 1_024

    let shellCommand: String

    init(
        workspace: String,
        previewToolsPath: String,
        previewctlRelativePath: String,
        arguments: [String]
    ) throws {
        guard Self.isSafeWord(workspace, maximumBytes: 4_096),
              Self.isSafeWord(previewToolsPath, maximumBytes: 4_096),
              Self.isSafeRelativePath(previewctlRelativePath),
              !arguments.isEmpty,
              arguments.count <= 32,
              arguments.allSatisfy({
                  Self.isSafeWord($0, maximumBytes: Self.maximumWordBytes)
              })
        else {
            throw RemotePreviewControlError.invalidCommandConfiguration
        }

        let previewctlPath = Self.joinPath(
            previewToolsPath,
            previewctlRelativePath
        )
        let previewctlCommand = ([
            "node",
            "--",
            Self.pathExpression(previewctlPath),
        ] + arguments.map(Self.shellQuote)).joined(separator: " ")
        let inner = [
            "cd -- \(Self.workspaceExpression(workspace))",
            previewctlCommand,
        ].joined(separator: " && ")
        let rendered = "exec \"$SHELL\" -lc \(Self.shellQuote(inner))"
        guard rendered.utf8.count <= Self.maximumCommandBytes else {
            throw RemotePreviewControlError.invalidCommandConfiguration
        }
        shellCommand = rendered
    }

    static func list(using builder: RemotePreviewCommandBuilder) throws -> Self {
        try Self(
            workspace: builder.workspacePath,
            previewToolsPath: builder.previewToolsPath,
            previewctlRelativePath: builder.previewctlRelativePath,
            arguments: ["list", "--workspace", ".", "--json"]
        )
    }

    static func status(
        sessionID: String,
        using builder: RemotePreviewCommandBuilder
    ) throws -> Self {
        guard isSessionIdentifier(sessionID) else {
            throw RemotePreviewControlError.invalidSessionID
        }
        return try Self(
            workspace: builder.workspacePath,
            previewToolsPath: builder.previewToolsPath,
            previewctlRelativePath: builder.previewctlRelativePath,
            arguments: ["status", "--session", sessionID, "--json"]
        )
    }

    static func issueToken(
        sessionID: String,
        remotePort: UInt16,
        localPort: UInt16,
        profile: PreviewQualityProfile,
        presentation: PreviewPresentation,
        using builder: RemotePreviewCommandBuilder
    ) throws -> Self {
        guard isSessionIdentifier(sessionID), remotePort > 0, localPort > 0 else {
            throw RemotePreviewControlError.invalidSessionID
        }
        return try Self(
            workspace: builder.workspacePath,
            previewToolsPath: builder.previewToolsPath,
            previewctlRelativePath: builder.previewctlRelativePath,
            arguments: [
                "describe",
                "--session", sessionID,
                "--issue-token",
                "--local-port", String(localPort),
                "--expected-remote-port", String(remotePort),
                "--profile", profile.rawValue,
                "--presentation", presentation.rawValue,
                "--json",
            ]
        )
    }

    private static func isSafeWord(
        _ value: String,
        maximumBytes: Int
    ) -> Bool {
        !value.isEmpty
            && value.utf8.count <= maximumBytes
            && !value.unicodeScalars.contains(where: {
                CharacterSet.controlCharacters.contains($0)
            })
    }

    private static func isSafeRelativePath(_ value: String) -> Bool {
        guard isSafeWord(value, maximumBytes: 1_024),
              !value.hasPrefix("/")
        else {
            return false
        }
        return value.split(separator: "/", omittingEmptySubsequences: false)
            .allSatisfy { component in
                !component.isEmpty && component != "." && component != ".."
            }
    }

    private static func isSessionIdentifier(_ value: String) -> Bool {
        guard (1 ... 128).contains(value.utf8.count),
              let first = value.unicodeScalars.first
        else {
            return false
        }
        let leading = CharacterSet(
            charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        )
        let remaining = CharacterSet(
            charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-"
        )
        return leading.contains(first)
            && value.unicodeScalars.allSatisfy(remaining.contains)
    }

    private static func shellQuote(_ value: String) -> String {
        "'\(value.replacingOccurrences(of: "'", with: "'\"'\"'"))'"
    }

    private static func workspaceExpression(_ workspace: String) -> String {
        pathExpression(workspace)
    }

    private static func pathExpression(_ path: String) -> String {
        if path == "~" {
            return "\"$HOME\""
        }
        if path.hasPrefix("~/") {
            let suffix = String(path.dropFirst(2))
            return suffix.isEmpty
                ? "\"$HOME\""
                : "\"$HOME\"/\(shellQuote(suffix))"
        }
        return shellQuote(path)
    }

    private static func joinPath(_ root: String, _ relative: String) -> String {
        if root == "." {
            return "./\(relative)"
        }
        if root.hasSuffix("/") {
            return root + relative
        }
        return root + "/" + relative
    }
}

/// Narrow preview seam implemented by the same authenticated SSH transport
/// that owns the interactive terminal. Exec output is bounded while reading,
/// and every accepted local socket gets one direct-tcpip SSH child.
protocol PreviewSSHSession: AnyObject, Sendable {
    var eventLoopGroup: EventLoopGroup { get }

    func runCommand(
        _ command: String,
        maximumOutputBytes: Int
    ) async throws -> Data

    func connectDirectTCPIP(
        localChannel: Channel,
        destination: SSHForwardDestination
    ) -> EventLoopFuture<Void>
}

extension SSHTransport: PreviewSSHSession {}

protocol PreviewLocalForwardListening: AnyObject, Sendable {
    func start() async throws -> UInt16
    func close() async
}

extension SSHLocalForward: PreviewLocalForwardListening {}

protocol PreviewLocalForwardCreating: Sendable {
    func makeForward(
        destination: SSHForwardDestination
    ) throws -> any PreviewLocalForwardListening
}

struct SSHSessionPreviewForwardFactory: PreviewLocalForwardCreating {
    let ssh: any PreviewSSHSession

    func makeForward(
        destination: SSHForwardDestination
    ) throws -> any PreviewLocalForwardListening {
        return SSHLocalForward(
            eventLoopGroup: ssh.eventLoopGroup,
            destination: destination
        ) { localChannel, destination in
            ssh.connectDirectTCPIP(
                localChannel: localChannel,
                destination: destination
            )
        }
    }
}

@MainActor
protocol PreviewAttachmentPresenting: AnyObject {
    /// Implementations make the latest `open` or `close` call authoritative;
    /// replacing an attachment invalidates any older in-flight open.
    func open(_ configuration: PreviewLaunchConfiguration) async throws
    func setPresentation(_ presentation: PreviewPresentation)
    func close()
}

extension PreviewSurfaceModel: PreviewAttachmentPresenting {}

enum PreviewCoordinatorState: Equatable, Sendable {
    case idle
    case discovering
    case startingTunnel
    case issuingToken
    case attached(sessionID: String, localPort: UInt16)
    case failed(String)
}

@MainActor
final class PreviewCoordinator: ObservableObject {
    @Published private(set) var state: PreviewCoordinatorState = .idle
    @Published private(set) var sessions: [RemotePreviewSession] = []
    private(set) var attachedSessionID: String?
    private(set) var attachedLocalPort: UInt16?

    private let ssh: any PreviewSSHSession
    private let commands: RemotePreviewCommandBuilder
    private let previewPresenter: any PreviewAttachmentPresenting
    private let forwardFactory: any PreviewLocalForwardCreating
    private var forward: ForwardLease?
    private var inFlightForward: ForwardLease?
    private var inFlightCommand: CommandLease?
    private var inFlightPresentationLeaseID: UUID?
    private var operationEpoch: UInt64 = 0

    private struct ForwardLease {
        let id = UUID()
        let listener: any PreviewLocalForwardListening
    }

    private struct CommandLease {
        let id = UUID()
        let task: Task<Data, Error>
    }

    init(
        ssh: any PreviewSSHSession,
        commands: RemotePreviewCommandBuilder,
        previewPresenter: any PreviewAttachmentPresenting,
        forwardFactory: any PreviewLocalForwardCreating
    ) {
        self.ssh = ssh
        self.commands = commands
        self.previewPresenter = previewPresenter
        self.forwardFactory = forwardFactory
    }

    convenience init(
        ssh: any PreviewSSHSession,
        commands: RemotePreviewCommandBuilder,
        previewModel: PreviewSurfaceModel
    ) {
        self.init(
            ssh: ssh,
            commands: commands,
            previewPresenter: previewModel,
            forwardFactory: SSHSessionPreviewForwardFactory(ssh: ssh)
        )
    }

    @discardableResult
    func refreshSessions() async throws -> [RemotePreviewSession] {
        let epoch = await beginOperation()
        guard epoch == operationEpoch else { return sessions }
        state = .discovering

        do {
            let command = try PreviewRemoteCommand.list(using: commands)
            let response = try await runRemoteCommand(
                command.shellCommand,
                maximumOutputBytes: RemotePreviewSessionList.maximumResponseBytes,
                epoch: epoch
            )
            let discovered = try RemotePreviewSessionList.parse(response)
                .filter(\.isAttachable)
                .sorted { $0.id < $1.id }
            guard epoch == operationEpoch else { return sessions }
            sessions = discovered
            restoreCommittedAttachmentState()
            return discovered
        } catch {
            guard epoch == operationEpoch else { throw error }
            if hasCommittedAttachment {
                restoreCommittedAttachmentState()
            } else {
                state = .failed(error.localizedDescription)
            }
            throw error
        }
    }

    func attach(
        sessionID: String,
        profile: PreviewQualityProfile = .mini,
        presentation: PreviewPresentation = .mini
    ) async throws {
        let epoch = await beginOperation()
        guard epoch == operationEpoch else { return }

        do {
            // Discover the current remote numeric-loopback port without
            // minting a credential. A list row can be stale by the time the
            // user taps it, so attach always uses an immediate status read.
            state = .discovering
            let statusCommand = try PreviewRemoteCommand.status(
                    sessionID: sessionID,
                    using: commands
                )
            let statusData = try await runRemoteCommand(
                statusCommand.shellCommand,
                maximumOutputBytes: RemotePreviewSessionList.maximumResponseBytes,
                epoch: epoch
            )
            let endpoint = try PreviewRemoteEndpoint.parseStatus(statusData)
            guard endpoint.sessionID == sessionID else {
                throw RemotePreviewControlError.invalidSessionID
            }
            guard epoch == operationEpoch else { return }

            state = .startingTunnel
            let destination = try SSHForwardDestination(
                host: "127.0.0.1",
                port: Int(endpoint.port)
            )
            let newForward = try forwardFactory.makeForward(
                destination: destination
            )
            let lease = ForwardLease(listener: newForward)
            inFlightForward = lease

            do {
                // `start()` returns only after ServerBootstrap has bound exact
                // 127.0.0.1:0 and resolved its numeric ephemeral port.
                let localPort = try await newForward.start()
                guard ownsInFlight(lease, epoch: epoch) else {
                    return
                }

                // The one-use token is intentionally minted after the tunnel
                // is listening, never before it.
                state = .issuingToken
                let tokenCommand = try PreviewRemoteCommand.issueToken(
                        sessionID: sessionID,
                        remotePort: endpoint.port,
                        localPort: localPort,
                        profile: profile,
                        presentation: presentation,
                        using: commands
                    )
                let descriptorData = try await runRemoteCommand(
                    tokenCommand.shellCommand,
                    maximumOutputBytes: PreviewLaunchConfiguration.maximumEncodedPayloadBytes,
                    epoch: epoch
                )
                let configuration = try PreviewLaunchConfiguration.parse(
                    descriptorData: descriptorData
                )
                guard configuration.session.sessionID == sessionID,
                      configuration.session.localPort == localPort,
                      configuration.session.profile == profile,
                      configuration.presentation == presentation
                else {
                    throw RemotePreviewControlError.localPortMismatch
                }
                guard ownsInFlight(lease, epoch: epoch) else {
                    return
                }

                inFlightPresentationLeaseID = lease.id
                do {
                    try await previewPresenter.open(configuration)
                } catch {
                    guard ownsInFlight(lease, epoch: epoch) else { return }
                    // PreviewSurfaceModel detaches the old receiver before it
                    // attempts the new one, so its old forward is no longer
                    // useful after a presentation failure.
                    let oldForward = forward
                    forward = nil
                    clearCommittedAttachment()
                    if inFlightPresentationLeaseID == lease.id {
                        inFlightPresentationLeaseID = nil
                    }
                    await retireInFlight(lease)
                    await oldForward?.listener.close()
                    throw error
                }
                guard ownsInFlight(lease, epoch: epoch) else {
                    // A newer attach or detach owns the presenter now. Closing
                    // it here could tear down that newer surface.
                    return
                }
                if inFlightPresentationLeaseID == lease.id {
                    inFlightPresentationLeaseID = nil
                }

                let oldForward = forward
                inFlightForward = nil
                forward = lease
                attachedSessionID = sessionID
                attachedLocalPort = localPort
                await oldForward?.listener.close()
                guard epoch == operationEpoch,
                      forward?.id == lease.id
                else {
                    return
                }
                state = .attached(
                    sessionID: sessionID,
                    localPort: localPort
                )
            } catch {
                await retireInFlight(lease)
                throw error
            }
        } catch {
            guard epoch == operationEpoch else { return }
            if hasCommittedAttachment {
                restoreCommittedAttachmentState()
            } else {
                state = .failed(error.localizedDescription)
            }
            throw error
        }
    }

    func setPresentation(_ presentation: PreviewPresentation) {
        previewPresenter.setPresentation(presentation)
    }

    func detach() async {
        operationEpoch &+= 1
        let epoch = operationEpoch
        let supersededCommand = inFlightCommand
        supersededCommand?.task.cancel()
        previewPresenter.close()
        let oldForward = forward
        let pendingForward = inFlightForward
        forward = nil
        inFlightForward = nil
        inFlightPresentationLeaseID = nil
        clearCommittedAttachment()
        await pendingForward?.listener.close()
        if oldForward?.id != pendingForward?.id {
            await oldForward?.listener.close()
        }
        if let supersededCommand {
            _ = try? await supersededCommand.task.value
            if inFlightCommand?.id == supersededCommand.id {
                inFlightCommand = nil
            }
        }
        guard epoch == operationEpoch else { return }
        sessions = []
        state = .idle
    }

    /// Starts a new coordinator intent and synchronously takes ownership away
    /// from any older listener that had not yet reached presentation commit.
    /// The close is awaited before the new operation can mint a token.
    private func beginOperation() async -> UInt64 {
        operationEpoch &+= 1
        let epoch = operationEpoch
        let supersededCommand = inFlightCommand
        supersededCommand?.task.cancel()
        let superseded = inFlightForward
        inFlightForward = nil

        // If the prior operation already entered presenter.open(), that call
        // detached the previously committed receiver. Invalidate the pending
        // open now and retire its formerly committed forward; otherwise a late
        // open could leave a live stale receiver when the newer attach fails
        // before reaching presentation.
        let supersededPresentationID = inFlightPresentationLeaseID
        let supersededCommittedForward: ForwardLease?
        if supersededPresentationID != nil {
            previewPresenter.close()
            inFlightPresentationLeaseID = nil
            supersededCommittedForward = forward
            forward = nil
            clearCommittedAttachment()
        } else {
            supersededCommittedForward = nil
        }

        await superseded?.listener.close()
        if supersededCommittedForward?.id != superseded?.id {
            await supersededCommittedForward?.listener.close()
        }

        // Replacement operations are serialized behind the previous exec
        // result. In particular, previewctl token issuance revokes older
        // generations; allowing a cancelled older command to finish after a
        // replacement would invalidate the replacement's one-use token.
        if let supersededCommand {
            _ = try? await supersededCommand.task.value
            if inFlightCommand?.id == supersededCommand.id {
                inFlightCommand = nil
            }
        }
        return epoch
    }

    private func ownsInFlight(_ lease: ForwardLease, epoch: UInt64) -> Bool {
        epoch == operationEpoch && inFlightForward?.id == lease.id
    }

    private func retireInFlight(_ lease: ForwardLease) async {
        guard inFlightForward?.id == lease.id else { return }
        inFlightForward = nil
        await lease.listener.close()
    }

    private var hasCommittedAttachment: Bool {
        forward != nil && attachedSessionID != nil && attachedLocalPort != nil
    }

    private func restoreCommittedAttachmentState() {
        guard let sessionID = attachedSessionID,
              let localPort = attachedLocalPort,
              forward != nil
        else {
            clearCommittedAttachment()
            state = .idle
            return
        }
        state = .attached(sessionID: sessionID, localPort: localPort)
    }

    private func clearCommittedAttachment() {
        attachedSessionID = nil
        attachedLocalPort = nil
    }

    private func runRemoteCommand(
        _ command: String,
        maximumOutputBytes: Int,
        epoch: UInt64
    ) async throws -> Data {
        guard epoch == operationEpoch else {
            throw CancellationError()
        }
        let ssh = self.ssh
        let task = Task {
            try await ssh.runCommand(
                command,
                maximumOutputBytes: maximumOutputBytes
            )
        }
        let lease = CommandLease(task: task)
        inFlightCommand = lease
        defer {
            if inFlightCommand?.id == lease.id {
                inFlightCommand = nil
            }
        }
        return try await withTaskCancellationHandler {
            try await task.value
        } onCancel: {
            task.cancel()
        }
    }
}

private struct PreviewRemoteEndpoint: Equatable, Sendable {
    let sessionID: String
    let port: UInt16

    static func parseStatus(_ data: Data) throws -> Self {
        guard !data.isEmpty,
              data.count <= RemotePreviewSessionList.maximumResponseBytes,
              let object = try? JSONSerialization.jsonObject(with: data),
              let payload = object as? [String: Any]
        else {
            throw RemotePreviewControlError.invalidResponse
        }

        let allowedKeys: Set<String> = [
            "protocolVersion", "sessionId", "ensureKey", "runId",
            "generation", "activeGeneration", "workspace", "tmux",
            "target", "signaling", "state", "heartbeatAt", "daemon",
            "lastError", "health",
        ]
        guard Set(payload.keys).isSubset(of: allowedKeys),
              integer(payload["protocolVersion"])
                  == PreviewSessionDescriptor.supportedProtocolVersion,
              let sessionID = payload["sessionId"] as? String,
              let state = payload["state"] as? String,
              ["ready", "connected"].contains(state),
              let signaling = payload["signaling"] as? [String: Any],
              Set(signaling.keys) == ["host", "port", "path"],
              signaling["host"] as? String == "127.0.0.1",
              signaling["path"] as? String == "/signal",
              let rawPort = integer(signaling["port"]),
              let port = UInt16(exactly: rawPort),
              port > 0
        else {
            throw RemotePreviewControlError.nonLoopbackRemote
        }

        guard let health = payload["health"] as? [String: Any],
              Set(health.keys) == [
                  "heartbeatFresh", "tmuxAlive", "active",
              ],
              health["heartbeatFresh"] as? Bool == true,
              health["tmuxAlive"] as? Bool == true,
              health["active"] as? Bool == true
        else {
            throw RemotePreviewControlError.invalidResponse
        }

        return Self(sessionID: sessionID, port: port)
    }

    private static func integer(_ value: Any?) -> Int? {
        guard let number = value as? NSNumber,
              CFGetTypeID(number) != CFBooleanGetTypeID()
        else {
            return nil
        }
        let double = number.doubleValue
        guard double.isFinite,
              double.rounded(.towardZero) == double,
              double >= Double(Int.min),
              double <= Double(Int.max)
        else {
            return nil
        }
        return Int(double)
    }
}
