import Foundation

protocol SSHCommandRunning: AnyObject {
    func runCommand(_ command: String, maximumOutputBytes: Int) async throws -> Data
}

struct RemotePreviewSession: Equatable, Identifiable, Sendable {
    let id: String
    let state: String
    let remoteSignalingPort: UInt16?
    let sourceWidth: Int?
    let sourceHeight: Int?
    let healthActive: Bool

    init(
        id: String,
        state: String,
        remoteSignalingPort: UInt16?,
        sourceWidth: Int?,
        sourceHeight: Int?,
        healthActive: Bool = true
    ) {
        self.id = id
        self.state = state
        self.remoteSignalingPort = remoteSignalingPort
        self.sourceWidth = sourceWidth
        self.sourceHeight = sourceHeight
        self.healthActive = healthActive
    }

    var isAttachable: Bool {
        ["ready", "connected"].contains(state)
            && remoteSignalingPort != nil
            && healthActive
    }
}

enum RemotePreviewSessionList {
    static let maximumResponseBytes = 1_048_576
    static let maximumSessions = 256

    static func parse(_ data: Data) throws -> [RemotePreviewSession] {
        guard data.count <= maximumResponseBytes else {
            throw RemotePreviewControlError.responseTooLarge
        }
        try validateStrictStructure(data)

        let decoded: SessionListPayload
        do {
            decoded = try JSONDecoder().decode(SessionListPayload.self, from: data)
        } catch {
            throw RemotePreviewControlError.invalidResponse
        }
        guard decoded.protocolVersion == PreviewSessionDescriptor.supportedProtocolVersion,
              decoded.sessions.count <= maximumSessions
        else {
            throw RemotePreviewControlError.invalidResponse
        }

        return try decoded.sessions.map { session in
            guard isSessionIdentifier(session.sessionID),
                  isState(session.state)
            else {
                throw RemotePreviewControlError.invalidResponse
            }

            let signalingPort: UInt16?
            if let signaling = session.signaling {
                guard signaling.host == "127.0.0.1",
                      signaling.path == "/signal",
                      let port = UInt16(exactly: signaling.port),
                      port > 0
                else {
                    throw RemotePreviewControlError.nonLoopbackRemote
                }
                signalingPort = port
            } else {
                signalingPort = nil
            }

            return RemotePreviewSession(
                id: session.sessionID,
                state: session.state,
                remoteSignalingPort: signalingPort,
                sourceWidth: boundedDimension(session.target?.sourceWidth),
                sourceHeight: boundedDimension(session.target?.sourceHeight),
                healthActive: session.health.active
                    && session.health.heartbeatFresh
                    && session.health.tmuxAlive
            )
        }
    }

    private static func isSessionIdentifier(_ value: String) -> Bool {
        guard (1 ... 128).contains(value.utf8.count),
              let first = value.unicodeScalars.first
        else {
            return false
        }
        let leading = CharacterSet.alphanumerics
        let remaining = CharacterSet(
            charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-"
        )
        return leading.contains(first) && value.unicodeScalars.allSatisfy(remaining.contains)
    }

    private static func isState(_ value: String) -> Bool {
        [
            "starting", "ready", "connected", "error", "stopping",
            "stopped", "stale",
        ].contains(value)
    }

    private static func boundedDimension(_ value: Int?) -> Int? {
        guard let value, (1 ... 16_384).contains(value) else { return nil }
        return value
    }

    private static func validateStrictStructure(_ data: Data) throws {
        guard let object = try? JSONSerialization.jsonObject(with: data),
              let payload = object as? [String: Any],
              Set(payload.keys) == ["protocolVersion", "sessions"],
              let sessions = payload["sessions"] as? [Any],
              sessions.count <= maximumSessions
        else {
            throw RemotePreviewControlError.invalidResponse
        }

        let allowedSessionKeys: Set<String> = [
            "protocolVersion", "sessionId", "ensureKey", "runId",
            "generation", "activeGeneration", "workspace", "tmux",
            "target", "signaling", "state", "heartbeatAt", "daemon",
            "lastError", "health",
        ]
        for value in sessions {
            guard let session = value as? [String: Any],
                  Set(session.keys).isSubset(of: allowedSessionKeys),
                  session["sessionId"] is String,
                  session["state"] is String,
                  let health = session["health"] as? [String: Any],
                  Set(health.keys) == [
                      "heartbeatFresh", "tmuxAlive", "active",
                  ],
                  health["heartbeatFresh"] is Bool,
                  health["tmuxAlive"] is Bool,
                  health["active"] is Bool
            else {
                throw RemotePreviewControlError.invalidResponse
            }
            if let signaling = session["signaling"],
               !(signaling is NSNull) {
                guard let signaling = signaling as? [String: Any],
                      Set(signaling.keys) == ["host", "port", "path"]
                else {
                    throw RemotePreviewControlError.invalidResponse
                }
            }
            if let workspace = session["workspace"] {
                guard let workspace = workspace as? [String: Any],
                      Set(workspace.keys) == ["canonicalRoot", "revision"]
                else {
                    throw RemotePreviewControlError.invalidResponse
                }
            }
            if let tmux = session["tmux"] {
                guard let tmux = tmux as? [String: Any],
                      Set(tmux.keys) == ["session", "pane", "owned"]
                else {
                    throw RemotePreviewControlError.invalidResponse
                }
            }
            if let target = session["target"] {
                guard let target = target as? [String: Any],
                      Set(target.keys).isSubset(of: [
                          "id", "urlHash", "canvasSelector",
                          "sourceWidth", "sourceHeight",
                      ])
                else {
                    throw RemotePreviewControlError.invalidResponse
                }
            }
        }
    }
}

struct RemotePreviewCommandBuilder: Equatable, Sendable {
    let workspacePath: String
    let previewToolsPath: String
    let previewctlRelativePath: String

    init(
        workspacePath: String,
        previewToolsPath: String = ".",
        previewctlRelativePath: String = "previewd/bin/previewctl.mjs"
    ) throws {
        guard !workspacePath.isEmpty,
              workspacePath.utf8.count <= 4_096,
              !workspacePath.unicodeScalars.contains(
                  where: CharacterSet.controlCharacters.contains
              ),
              !previewToolsPath.isEmpty,
              previewToolsPath.utf8.count <= 4_096,
              !previewToolsPath.unicodeScalars.contains(
                  where: CharacterSet.controlCharacters.contains
              ),
              Self.isSafeRelativePath(previewctlRelativePath)
        else {
            throw RemotePreviewControlError.invalidCommandConfiguration
        }
        self.workspacePath = workspacePath
        self.previewToolsPath = previewToolsPath
        self.previewctlRelativePath = previewctlRelativePath
    }

    func list() -> String {
        loginShellCommand([
            "node",
            "--",
            previewctlPathExpression,
            "list",
            "--workspace",
            POSIXShell.quote("."),
            "--json",
        ].joined(separator: " "))
    }

    func status(sessionID: String) throws -> String {
        try sessionCommand("status", sessionID: sessionID, additionalArguments: [])
    }

    func describe(
        sessionID: String,
        remotePort: UInt16,
        localPort: UInt16,
        profile: PreviewQualityProfile,
        presentation: PreviewPresentation
    ) throws -> String {
        try sessionCommand(
            "describe",
            sessionID: sessionID,
            additionalArguments: [
                "--issue-token",
                "--local-port", String(localPort),
                "--expected-remote-port", String(remotePort),
                "--profile", profile.rawValue,
                "--presentation", presentation.rawValue,
            ]
        )
    }

    private func sessionCommand(
        _ verb: String,
        sessionID: String,
        additionalArguments: [String]
    ) throws -> String {
        guard RemotePreviewSessionList.isValidSessionIdentifier(sessionID) else {
            throw RemotePreviewControlError.invalidSessionID
        }
        let command = ([
            "node",
            "--",
            previewctlPathExpression,
            verb,
            "--session",
            POSIXShell.quote(sessionID),
        ] + additionalArguments + ["--json"]).joined(separator: " ")
        return loginShellCommand(command)
    }

    private func loginShellCommand(_ command: String) -> String {
        let inner = "cd -- \(POSIXShell.pathExpression(workspacePath)) && \(command)"
        return "exec \"$SHELL\" -lc \(POSIXShell.quote(inner))"
    }

    /// The tools checkout may be separate from the project being edited. A
    /// relative tools path is resolved from the workspace after `cd`; absolute
    /// and `~/` paths retain their usual remote-shell meaning without exposing
    /// either value as shell syntax.
    private var previewctlPathExpression: String {
        POSIXShell.pathExpression(
            Self.joinPath(previewToolsPath, previewctlRelativePath)
        )
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

    private static func isSafeRelativePath(_ value: String) -> Bool {
        guard !value.isEmpty,
              !value.hasPrefix("/"),
              value.utf8.count <= 1_024,
              !value.unicodeScalars.contains(
                  where: CharacterSet.controlCharacters.contains
              )
        else {
            return false
        }
        return value.split(separator: "/", omittingEmptySubsequences: false)
            .allSatisfy { component in
                !component.isEmpty && component != "." && component != ".."
            }
    }
}

enum POSIXShell {
    static func quote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// Quotes a path while preserving the one shell expansion users expect in
    /// remote profiles: a leading `~` means that SSH account's home directory.
    /// Every user-controlled suffix remains a single literal shell word.
    static func pathExpression(_ value: String) -> String {
        if value == "~" {
            return "\"$HOME\""
        }
        if value.hasPrefix("~/") {
            let suffix = String(value.dropFirst(2))
            return suffix.isEmpty ? "\"$HOME\"" : "\"$HOME\"/\(quote(suffix))"
        }
        return quote(value)
    }
}

enum RemotePreviewControlError: Error, Equatable, LocalizedError {
    case invalidResponse
    case responseTooLarge
    case nonLoopbackRemote
    case invalidCommandConfiguration
    case invalidSessionID
    case localPortMismatch

    var errorDescription: String? {
        switch self {
        case .invalidResponse:
            return "The remote preview session response is malformed."
        case .responseTooLarge:
            return "The remote preview response exceeded its size limit."
        case .nonLoopbackRemote:
            return "The remote preview service is not bound to exact loopback."
        case .invalidCommandConfiguration:
            return "The remote workspace or previewctl path is invalid."
        case .invalidSessionID:
            return "The preview session identifier is invalid."
        case .localPortMismatch:
            return "The preview descriptor does not match the active local tunnel."
        }
    }
}

private struct SessionListPayload: Decodable {
    let protocolVersion: Int
    let sessions: [SessionPayload]
}

private struct SessionPayload: Decodable {
    let sessionID: String
    let state: String
    let signaling: SignalingPayload?
    let target: TargetPayload?
    let health: HealthPayload

    private enum CodingKeys: String, CodingKey {
        case sessionID = "sessionId"
        case state
        case signaling
        case target
        case health
    }
}

private struct SignalingPayload: Decodable {
    let host: String
    let port: Int
    let path: String
}

private struct TargetPayload: Decodable {
    let sourceWidth: Int?
    let sourceHeight: Int?
}

private struct HealthPayload: Decodable {
    let heartbeatFresh: Bool
    let tmuxAlive: Bool
    let active: Bool
}

private extension RemotePreviewSessionList {
    static func isValidSessionIdentifier(_ value: String) -> Bool {
        isSessionIdentifier(value)
    }
}
