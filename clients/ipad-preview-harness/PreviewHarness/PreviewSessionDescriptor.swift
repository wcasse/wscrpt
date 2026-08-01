import Foundation

enum PreviewConfigurationError: Error, Equatable, LocalizedError {
    case invalidDeepLink
    case payloadTooLarge
    case invalidPayload
    case unknownField(String)
    case unsupportedProtocolVersion(Int)
    case invalidSessionID
    case invalidGeneration
    case invalidNonce
    case invalidToken
    case invalidLocalPort
    case invalidProfile
    case invalidPresentation
    case invalidProvider
    case nonLoopbackURL

    var errorDescription: String? {
        switch self {
        case .invalidDeepLink:
            return "The preview link is not in the expected format."
        case .payloadTooLarge:
            return "The preview descriptor is too large."
        case .invalidPayload:
            return "The preview descriptor is malformed."
        case let .unknownField(field):
            return "The preview descriptor contains an unsupported field: \(field)."
        case let .unsupportedProtocolVersion(version):
            return "Preview protocol version \(version) is not supported."
        case .invalidSessionID:
            return "The preview session identifier is invalid."
        case .invalidGeneration:
            return "The preview generation is invalid."
        case .invalidNonce:
            return "The preview nonce is invalid."
        case .invalidToken:
            return "The preview token is invalid."
        case .invalidLocalPort:
            return "The forwarded preview port is invalid."
        case .invalidProfile:
            return "The preview quality profile is invalid."
        case .invalidPresentation:
            return "The preview presentation is invalid."
        case .invalidProvider:
            return "The preview provider is invalid."
        case .nonLoopbackURL:
            return "The preview endpoint must be on this iPad's loopback interface."
        }
    }
}

enum PreviewPresentation: String, Codable, CaseIterable, Sendable {
    case mini
    case expanded
}

enum PreviewQualityProfile: String, Codable, CaseIterable, Sendable {
    case mini
    case expanded
    case expandedHeadroom = "expanded-headroom"
    case fallback
}

struct PreviewSessionDescriptor: Equatable, Sendable {
    static let supportedProtocolVersion = 1
    static let maximumJavaScriptInteger: UInt64 = 9_007_199_254_740_991

    let protocolVersion: Int
    let sessionID: String
    let generation: UInt64
    let nonce: String
    let token: String
    let localPort: UInt16
    let profile: PreviewQualityProfile

    init(
        protocolVersion: Int,
        sessionID: String,
        generation: UInt64,
        nonce: String,
        token: String,
        signalingURL: URL,
        profile: PreviewQualityProfile
    ) throws {
        guard protocolVersion == Self.supportedProtocolVersion else {
            throw PreviewConfigurationError.unsupportedProtocolVersion(protocolVersion)
        }
        guard Self.isIdentifier(sessionID, minimumLength: 1, maximumLength: 128) else {
            throw PreviewConfigurationError.invalidSessionID
        }
        guard generation > 0, generation <= Self.maximumJavaScriptInteger else {
            throw PreviewConfigurationError.invalidGeneration
        }
        // Keep the native gate identical to previewd's protocol grammar so a
        // descriptor cannot pass Swift validation and fail only after WebKit
        // has loaded the one-use credential.
        guard Self.isOpaqueCredential(nonce, minimumLength: 16, maximumLength: 128) else {
            throw PreviewConfigurationError.invalidNonce
        }
        guard Self.isOpaqueCredential(token, minimumLength: 32, maximumLength: 256) else {
            throw PreviewConfigurationError.invalidToken
        }
        guard Self.isStrictLoopbackSignalingURL(signalingURL),
              let signalingPort = signalingURL.port,
              let localPort = UInt16(exactly: signalingPort)
        else {
            throw PreviewConfigurationError.invalidLocalPort
        }

        self.protocolVersion = protocolVersion
        self.sessionID = sessionID
        self.generation = generation
        self.nonce = nonce
        self.token = token
        self.localPort = localPort
        self.profile = profile
    }

    var playerURL: URL {
        var components = URLComponents()
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = Int(localPort)
        components.path = "/"
        return components.url!
    }

    var signalingURL: URL {
        var components = URLComponents()
        components.scheme = "ws"
        components.host = "127.0.0.1"
        components.port = Int(localPort)
        components.path = "/signal"
        return components.url!
    }

    func browserConfiguration(presentation: PreviewPresentation) -> [String: Any] {
        [
            "protocolVersion": protocolVersion,
            "sessionId": sessionID,
            "generation": NSNumber(value: generation),
            "nonce": nonce,
            "token": token,
            "signaling": ["url": signalingURL.absoluteString],
            "profile": profile.rawValue,
            "provider": "webrtc",
            "presentation": presentation.rawValue,
        ]
    }

    func receiverURL(presentation: PreviewPresentation) throws -> URL {
        let configuration = browserConfiguration(presentation: presentation)
        guard JSONSerialization.isValidJSONObject(configuration),
              let data = try? JSONSerialization.data(
                  withJSONObject: configuration,
                  options: [.sortedKeys]
              )
        else {
            throw PreviewConfigurationError.invalidPayload
        }

        let encoded = data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        var components = URLComponents(url: playerURL, resolvingAgainstBaseURL: false)!
        components.fragment = "attach=\(encoded)"
        guard let url = components.url else {
            throw PreviewConfigurationError.invalidPayload
        }
        return url
    }

    static func isStrictLoopbackPageURL(_ url: URL) -> Bool {
        guard let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              components.scheme?.lowercased() == "http",
              components.host == "127.0.0.1",
              let port = components.port,
              (1 ... 65_535).contains(port),
              components.user == nil,
              components.password == nil,
              components.query == nil
        else {
            return false
        }

        return components.path == "/" && components.fragment == nil
    }

    static func isStrictLoopbackSignalingURL(_ url: URL) -> Bool {
        guard let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              components.scheme?.lowercased() == "ws",
              components.host == "127.0.0.1",
              let port = components.port,
              (1 ... 65_535).contains(port),
              components.user == nil,
              components.password == nil,
              components.query == nil,
              components.fragment == nil
        else {
            return false
        }

        return components.path == "/signal"
    }

    private static func isIdentifier(
        _ value: String,
        minimumLength: Int,
        maximumLength: Int
    ) -> Bool {
        guard (minimumLength ... maximumLength).contains(value.utf8.count) else {
            return false
        }
        let scalars = value.unicodeScalars
        let leadingCharacters = CharacterSet(
            charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        )
        let allCharacters = CharacterSet(
            charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-"
        )
        guard let first = scalars.first, leadingCharacters.contains(first) else {
            return false
        }
        return scalars.allSatisfy(allCharacters.contains)
    }

    private static func isOpaqueCredential(
        _ value: String,
        minimumLength: Int,
        maximumLength: Int
    ) -> Bool {
        guard (minimumLength ... maximumLength).contains(value.utf8.count) else {
            return false
        }
        return value.unicodeScalars.allSatisfy { scalar in
            CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-")
                .contains(scalar)
        }
    }
}
