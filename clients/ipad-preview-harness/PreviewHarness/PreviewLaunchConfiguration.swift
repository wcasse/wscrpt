import Foundation

struct PreviewLaunchConfiguration: Equatable, Sendable {
    static let scheme = "wscrpt-preview"
    static let host = "attach"
    static let maximumEncodedPayloadBytes = 16_384

    let session: PreviewSessionDescriptor
    let presentation: PreviewPresentation
    let encodedAttachment: String

    var playerURL: URL {
        var components = URLComponents(url: session.playerURL, resolvingAgainstBaseURL: false)!
        components.fragment = "attach=\(encodedAttachment)"
        return components.url!
    }

    static func parse(deepLink: URL) throws -> PreviewLaunchConfiguration {
        guard let components = URLComponents(url: deepLink, resolvingAgainstBaseURL: false),
              components.scheme?.lowercased() == scheme,
              components.host?.lowercased() == host,
              components.path.isEmpty,
              components.port == nil,
              components.user == nil,
              components.password == nil,
              components.query == nil,
              let fragment = components.fragment,
              components.percentEncodedFragment == fragment,
              fragment.hasPrefix("attach=")
        else {
            throw PreviewConfigurationError.invalidDeepLink
        }

        let encodedAttachment = String(fragment.dropFirst("attach=".count))
        guard !encodedAttachment.isEmpty,
              !encodedAttachment.contains("=")
        else {
            throw PreviewConfigurationError.invalidDeepLink
        }

        guard encodedAttachment.utf8.count <= maximumEncodedPayloadBytes else {
            throw PreviewConfigurationError.payloadTooLarge
        }
        guard encodedAttachment.unicodeScalars.allSatisfy({
            CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-")
                .contains($0)
        }) else {
            throw PreviewConfigurationError.invalidPayload
        }

        let payloadData = try decodeBase64URL(encodedAttachment)
        return try parse(
            payloadData: payloadData,
            encodedAttachment: encodedAttachment
        )
    }

    /// Parses the machine-readable output returned by `previewctl describe
    /// --issue-token --json`. The native SSH path deliberately shares the
    /// exact schema and endpoint checks used by the deep-link path.
    static func parse(descriptorData: Data) throws -> PreviewLaunchConfiguration {
        guard descriptorData.count <= maximumEncodedPayloadBytes else {
            throw PreviewConfigurationError.payloadTooLarge
        }
        let encodedAttachment = base64URLEncode(descriptorData)
        guard encodedAttachment.utf8.count <= maximumEncodedPayloadBytes else {
            throw PreviewConfigurationError.payloadTooLarge
        }
        return try parse(
            payloadData: descriptorData,
            encodedAttachment: encodedAttachment
        )
    }

    private static func parse(
        payloadData: Data,
        encodedAttachment: String
    ) throws -> PreviewLaunchConfiguration {
        guard payloadData.count <= maximumEncodedPayloadBytes,
              let object = try? JSONSerialization.jsonObject(with: payloadData),
              let dictionary = object as? [String: Any]
        else {
            throw PreviewConfigurationError.invalidPayload
        }

        let allowedKeys: Set<String> = [
            "protocolVersion",
            "sessionId",
            "generation",
            "nonce",
            "token",
            "signaling",
            "profile",
            "provider",
            "presentation",
        ]
        if let unknownKey = Set(dictionary.keys).subtracting(allowedKeys).sorted().first {
            throw PreviewConfigurationError.unknownField(unknownKey)
        }
        guard Set(dictionary.keys) == allowedKeys,
              let signaling = dictionary["signaling"] as? [String: Any],
              Set(signaling.keys) == ["url"]
        else {
            throw PreviewConfigurationError.invalidPayload
        }

        let decoded: LaunchPayload
        do {
            decoded = try JSONDecoder().decode(LaunchPayload.self, from: payloadData)
        } catch {
            throw PreviewConfigurationError.invalidPayload
        }

        guard let profile = PreviewQualityProfile(rawValue: decoded.profile) else {
            throw PreviewConfigurationError.invalidProfile
        }
        guard let presentation = PreviewPresentation(rawValue: decoded.presentation) else {
            throw PreviewConfigurationError.invalidPresentation
        }
        guard decoded.provider == "webrtc" else {
            throw PreviewConfigurationError.invalidProvider
        }
        guard let signalingURL = URL(string: decoded.signaling.url),
              PreviewSessionDescriptor.isStrictLoopbackSignalingURL(signalingURL)
        else {
            throw PreviewConfigurationError.nonLoopbackURL
        }

        let session = try PreviewSessionDescriptor(
            protocolVersion: decoded.protocolVersion,
            sessionID: decoded.sessionID,
            generation: decoded.generation,
            nonce: decoded.nonce,
            token: decoded.token,
            signalingURL: signalingURL,
            profile: profile
        )
        guard PreviewSessionDescriptor.isStrictLoopbackPageURL(session.playerURL) else {
            throw PreviewConfigurationError.nonLoopbackURL
        }

        return PreviewLaunchConfiguration(
            session: session,
            presentation: presentation,
            encodedAttachment: encodedAttachment
        )
    }

    private static func decodeBase64URL(_ encoded: String) throws -> Data {
        var base64 = encoded.replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        let remainder = base64.count % 4
        if remainder != 0 {
            base64.append(String(repeating: "=", count: 4 - remainder))
        }
        guard let decoded = Data(base64Encoded: base64),
              base64URLEncode(decoded) == encoded
        else {
            throw PreviewConfigurationError.invalidPayload
        }
        return decoded
    }

    private static func base64URLEncode(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}

private struct LaunchPayload: Decodable {
    let protocolVersion: Int
    let sessionID: String
    let generation: UInt64
    let nonce: String
    let token: String
    let signaling: SignalingPayload
    let profile: String
    let provider: String
    let presentation: String

    private enum CodingKeys: String, CodingKey {
        case protocolVersion
        case sessionID = "sessionId"
        case generation
        case nonce
        case token
        case signaling
        case profile
        case provider
        case presentation
    }
}

private struct SignalingPayload: Decodable {
    let url: String

    private enum CodingKeys: String, CodingKey {
        case url
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        guard container.allKeys.count == 1 else {
            throw DecodingError.dataCorrupted(
                .init(codingPath: decoder.codingPath, debugDescription: "Unexpected signaling field")
            )
        }
        url = try container.decode(String.self, forKey: .url)
    }
}
