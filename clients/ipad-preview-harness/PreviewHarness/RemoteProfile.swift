import Foundation

/// Non-secret connection metadata for one remote wscrpt workspace.
///
/// Passwords and generated private keys deliberately do not live in this
/// value. `id` is one stable component of the Keychain account; passwords add
/// normalized host, port, and username so editing an endpoint cannot reuse a
/// credential that belonged to another server.
struct RemoteProfile: Identifiable, Codable, Equatable, Sendable {
    let id: UUID
    let name: String
    let host: String
    let port: UInt16
    let username: String
    let workspace: String
    let previewToolsPath: String
    let launchStyle: RemoteLaunchStyle
    let authenticationMethod: RemoteAuthenticationMethod

    init(
        id: UUID = UUID(),
        name: String,
        host: String,
        port: UInt16 = 22,
        username: String,
        workspace: String = ".",
        previewToolsPath: String = ".",
        launchStyle: RemoteLaunchStyle = .tmux(session: "wscrpt"),
        authenticationMethod: RemoteAuthenticationMethod = .password
    ) throws {
        let normalizedHost = try Self.normalizeHost(host)
        let normalizedUsername = try Self.normalizeUsername(username)
        let normalizedWorkspace = try Self.normalizeWorkspace(workspace)
        let normalizedPreviewToolsPath = try Self.normalizePreviewToolsPath(previewToolsPath)
        let normalizedName = try Self.normalizeName(
            name,
            fallback: "\(normalizedUsername)@\(normalizedHost)"
        )

        guard port > 0 else {
            throw RemoteProfileValidationError.invalidPort
        }

        let normalizedLaunchStyle: RemoteLaunchStyle
        switch launchStyle {
        case .direct:
            normalizedLaunchStyle = .direct
        case let .tmux(session):
            let normalizedSession = session.trimmingCharacters(in: .whitespacesAndNewlines)
            guard Self.isValidTmuxSessionName(normalizedSession) else {
                throw RemoteProfileValidationError.invalidTmuxSession
            }
            normalizedLaunchStyle = .tmux(session: normalizedSession)
        }

        self.id = id
        self.name = normalizedName
        self.host = normalizedHost
        self.port = port
        self.username = normalizedUsername
        self.workspace = normalizedWorkspace
        self.previewToolsPath = normalizedPreviewToolsPath
        self.launchStyle = normalizedLaunchStyle
        self.authenticationMethod = authenticationMethod
    }

    var endpointDescription: String {
        let renderedHost = host.contains(":") ? "[\(host)]" : host
        return port == 22 ? renderedHost : "\(renderedHost):\(port)"
    }

    var connectionDescription: String {
        "\(username)@\(endpointDescription)"
    }

    private static func normalizeName(_ value: String, fallback: String) throws -> String {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        let effective = trimmed.isEmpty ? fallback : trimmed
        guard effective.utf8.count <= 96,
              !effective.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        else {
            throw RemoteProfileValidationError.invalidName
        }
        return effective
    }

    private static func normalizeHost(_ value: String) throws -> String {
        var trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.hasPrefix("["), trimmed.hasSuffix("]"), trimmed.count > 2 {
            trimmed.removeFirst()
            trimmed.removeLast()
        }

        let colonCount = trimmed.reduce(into: 0) { count, character in
            if character == ":" { count += 1 }
        }
        guard !trimmed.isEmpty,
              trimmed.utf8.count <= 1_024,
              colonCount != 1,
              !trimmed.contains("://"),
              !trimmed.contains("@"),
              !trimmed.contains("/"),
              !trimmed.contains("\\"),
              !trimmed.unicodeScalars.contains(where: {
                  CharacterSet.whitespacesAndNewlines.contains($0)
                      || CharacterSet.controlCharacters.contains($0)
              })
        else {
            throw RemoteProfileValidationError.invalidHost
        }

        return trimmed.lowercased()
    }

    private enum CodingKeys: String, CodingKey {
        case id
        case name
        case host
        case port
        case username
        case workspace
        case previewToolsPath
        case launchStyle
        case authenticationMethod
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(
            id: container.decode(UUID.self, forKey: .id),
            name: container.decode(String.self, forKey: .name),
            host: container.decode(String.self, forKey: .host),
            port: container.decode(UInt16.self, forKey: .port),
            username: container.decode(String.self, forKey: .username),
            workspace: container.decode(String.self, forKey: .workspace),
            previewToolsPath: try container.decodeIfPresent(
                String.self,
                forKey: .previewToolsPath
            ) ?? ".",
            launchStyle: container.decode(RemoteLaunchStyle.self, forKey: .launchStyle),
            authenticationMethod: container.decode(
                RemoteAuthenticationMethod.self,
                forKey: .authenticationMethod
            )
        )
    }

    private static func normalizeUsername(_ value: String) throws -> String {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              trimmed.utf8.count <= 255,
              !trimmed.unicodeScalars.contains(where: {
                  CharacterSet.whitespacesAndNewlines.contains($0)
                      || CharacterSet.controlCharacters.contains($0)
              })
        else {
            throw RemoteProfileValidationError.invalidUsername
        }
        return trimmed
    }

    private static func normalizeWorkspace(_ value: String) throws -> String {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard isValidRemotePath(trimmed) else {
            throw RemoteProfileValidationError.invalidWorkspace
        }
        return trimmed
    }

    private static func normalizePreviewToolsPath(_ value: String) throws -> String {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard isValidRemotePath(trimmed) else {
            throw RemoteProfileValidationError.invalidPreviewToolsPath
        }
        return trimmed
    }

    private static func isValidRemotePath(_ value: String) -> Bool {
        !value.isEmpty
            && value.utf8.count <= 4_096
            && !value.unicodeScalars.contains(
                where: CharacterSet.controlCharacters.contains
            )
    }

    private static func isValidTmuxSessionName(_ value: String) -> Bool {
        guard (1 ... 64).contains(value.utf8.count),
              let first = value.unicodeScalars.first
        else {
            return false
        }

        let leading = CharacterSet.alphanumerics
        let remaining = CharacterSet(
            charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-"
        )
        return leading.contains(first) && value.unicodeScalars.allSatisfy(remaining.contains)
    }
}

enum RemoteLaunchStyle: Codable, Equatable, Sendable {
    case direct
    case tmux(session: String)
}

enum RemoteAuthenticationMethod: String, Codable, CaseIterable, Identifiable, Sendable {
    case password
    case deviceKey

    var id: String { rawValue }

    var title: String {
        switch self {
        case .password:
            return "Password"
        case .deviceKey:
            return "Device key"
        }
    }
}

/// Mutable text-field representation. Conversion to `RemoteProfile` is the
/// single validation gate used by both the connection sheet and persistence.
struct RemoteProfileDraft: Equatable, Sendable {
    var id: UUID
    var name: String
    var host: String
    var port: String
    var username: String
    var workspace: String
    var previewToolsPath: String
    var usesTmux: Bool
    var tmuxSession: String
    var authenticationMethod: RemoteAuthenticationMethod

    init(
        id: UUID = UUID(),
        name: String = "",
        host: String = "",
        port: String = "22",
        username: String = "",
        workspace: String = ".",
        previewToolsPath: String = ".",
        usesTmux: Bool = true,
        tmuxSession: String = "wscrpt",
        authenticationMethod: RemoteAuthenticationMethod = .password
    ) {
        self.id = id
        self.name = name
        self.host = host
        self.port = port
        self.username = username
        self.workspace = workspace
        self.previewToolsPath = previewToolsPath
        self.usesTmux = usesTmux
        self.tmuxSession = tmuxSession
        self.authenticationMethod = authenticationMethod
    }

    init(profile: RemoteProfile) {
        id = profile.id
        name = profile.name
        host = profile.host
        port = String(profile.port)
        username = profile.username
        workspace = profile.workspace
        previewToolsPath = profile.previewToolsPath
        authenticationMethod = profile.authenticationMethod
        switch profile.launchStyle {
        case .direct:
            usesTmux = false
            tmuxSession = "wscrpt"
        case let .tmux(session):
            usesTmux = true
            tmuxSession = session
        }
    }

    func validatedProfile() throws -> RemoteProfile {
        let normalizedPort = port.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let integerPort = Int(normalizedPort),
              let port = UInt16(exactly: integerPort),
              port > 0
        else {
            throw RemoteProfileValidationError.invalidPort
        }

        return try RemoteProfile(
            id: id,
            name: name,
            host: host,
            port: port,
            username: username,
            workspace: workspace,
            previewToolsPath: previewToolsPath,
            launchStyle: usesTmux ? .tmux(session: tmuxSession) : .direct,
            authenticationMethod: authenticationMethod
        )
    }
}

enum RemoteProfileValidationError: Error, Equatable, LocalizedError {
    case invalidName
    case invalidHost
    case invalidPort
    case invalidUsername
    case invalidWorkspace
    case invalidPreviewToolsPath
    case invalidTmuxSession

    var errorDescription: String? {
        switch self {
        case .invalidName:
            return "Profile name must be 96 characters or fewer."
        case .invalidHost:
            return "Enter a host name or IP address without a scheme, user, or path."
        case .invalidPort:
            return "SSH port must be between 1 and 65535."
        case .invalidUsername:
            return "Enter an SSH user name without spaces or control characters."
        case .invalidWorkspace:
            return "Enter a remote workspace path without line breaks or control characters."
        case .invalidPreviewToolsPath:
            return "Enter the remote wscrpt tools path without line breaks or control characters."
        case .invalidTmuxSession:
            return "tmux session must be 1–64 letters, numbers, underscores, or hyphens."
        }
    }
}

/// Authentication material submitted by the sheet. An absent password means
/// “load the saved password for this profile”; it never means an empty SSH
/// password. Device identity generation and Keychain access belong to the app
/// session/transport adapter, not this UI value.
enum WorkspaceAuthenticationRequest: Equatable, Sendable {
    case password(value: String?, remember: Bool)
    case deviceKey
}

struct WorkspaceConnectionRequest: Equatable, Sendable {
    let profile: RemoteProfile
    let authentication: WorkspaceAuthenticationRequest
}

/// Builds the command sent after the SSH PTY is ready. It accepts only a
/// validated profile and quotes every value that reaches the remote shell.
enum RemoteLaunchCommandBuilder {
    static func command(for profile: RemoteProfile) -> String {
        let workspace = workspaceExpression(profile.workspace)
        let editorCommand = "exec wscrpt ."

        switch profile.launchStyle {
        case .direct:
            return "cd -- \(workspace) && \(editorCommand)"
        case let .tmux(session):
            return [
                "exec tmux new-session -A",
                "-s \(shellQuote(session))",
                "-c \(workspace)",
                shellQuote(editorCommand),
            ].joined(separator: " ")
        }
    }

    /// POSIX single-quote encoding. Kept internal so unit tests can exercise
    /// adversarial paths without launching a shell.
    static func shellQuote(_ value: String) -> String {
        "'\(value.replacingOccurrences(of: "'", with: "'\"'\"'"))'"
    }

    private static func workspaceExpression(_ workspace: String) -> String {
        if workspace == "~" {
            return "\"$HOME\""
        }
        if workspace.hasPrefix("~/") {
            let relativePath = String(workspace.dropFirst(2))
            return relativePath.isEmpty
                ? "\"$HOME\""
                : "\"$HOME\"/\(shellQuote(relativePath))"
        }
        return shellQuote(workspace)
    }
}
