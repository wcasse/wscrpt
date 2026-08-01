import Foundation
import XCTest
@testable import PreviewHarness

final class PreviewDescriptorTests: XCTestCase {
    func testCredentialBoundsMatchSignalingProtocol() throws {
        let signalingURL = try XCTUnwrap(URL(string: "ws://127.0.0.1:7331/signal"))

        XCTAssertThrowsError(
            try PreviewSessionDescriptor(
                protocolVersion: 1,
                sessionID: "credential-bounds",
                generation: 1,
                nonce: String(repeating: "n", count: 15),
                token: String(repeating: "t", count: 32),
                signalingURL: signalingURL,
                profile: .mini
            )
        ) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .invalidNonce)
        }

        XCTAssertThrowsError(
            try PreviewSessionDescriptor(
                protocolVersion: 1,
                sessionID: "credential-bounds",
                generation: 1,
                nonce: String(repeating: "n", count: 16),
                token: String(repeating: "t", count: 31),
                signalingURL: signalingURL,
                profile: .mini
            )
        ) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .invalidToken)
        }
    }

    func testSessionIdentifierMustStartWithASCIIAlphanumeric() throws {
        let signalingURL = try XCTUnwrap(URL(string: "ws://127.0.0.1:7331/signal"))

        for invalidSessionID in [".hidden", "_private", "-option"] {
            XCTAssertThrowsError(
                try PreviewSessionDescriptor(
                    protocolVersion: 1,
                    sessionID: invalidSessionID,
                    generation: 1,
                    nonce: "nonce_0123456789abcdef",
                    token: "token_0123456789abcdefghijklmnopqrstuvwxyz",
                    signalingURL: signalingURL,
                    profile: .mini
                )
            ) { error in
                XCTAssertEqual(error as? PreviewConfigurationError, .invalidSessionID)
            }
        }
    }

    func testParsesCanonicalLoopbackDescriptor() throws {
        let url = try makeDeepLink()
        let configuration = try PreviewLaunchConfiguration.parse(deepLink: url)

        XCTAssertEqual(configuration.session.protocolVersion, 1)
        XCTAssertEqual(configuration.session.sessionID, "session-019d")
        XCTAssertEqual(configuration.session.generation, 3)
        XCTAssertEqual(configuration.session.localPort, 7_331)
        XCTAssertEqual(configuration.session.profile, .mini)
        XCTAssertEqual(configuration.presentation, .mini)
        XCTAssertEqual(configuration.session.signalingURL.absoluteString, "ws://127.0.0.1:7331/signal")
        XCTAssertEqual(configuration.playerURL.host, "127.0.0.1")
        XCTAssertNil(URLComponents(url: configuration.playerURL, resolvingAgainstBaseURL: false)?.query)
        XCTAssertTrue(configuration.playerURL.fragment?.hasPrefix("attach=") == true)
        XCTAssertFalse(configuration.playerURL.absoluteString.contains("?"))
    }

    func testNativeSSHDescriptorUsesTheSameStrictContract() throws {
        let deepLink = try makeDeepLink()
        let fragment = try XCTUnwrap(
            URLComponents(url: deepLink, resolvingAgainstBaseURL: false)?.fragment
        )
        let payload = try decodeBase64URL(
            String(fragment.dropFirst("attach=".count))
        )

        let configuration = try PreviewLaunchConfiguration.parse(descriptorData: payload)

        XCTAssertEqual(configuration.session.sessionID, "session-019d")
        XCTAssertEqual(configuration.session.localPort, 7_331)
        XCTAssertEqual(configuration.presentation, .mini)
        XCTAssertNil(URLComponents(url: configuration.playerURL, resolvingAgainstBaseURL: false)?.query)
        XCTAssertTrue(configuration.playerURL.fragment?.hasPrefix("attach=") == true)
    }

    func testNativeSSHDescriptorRejectsExternalAndOversizedPayloads() throws {
        let external = try makeDeepLink(overrides: [
            "signaling": ["url": "ws://remotehost.local:7331/signal"],
        ])
        let fragment = try XCTUnwrap(
            URLComponents(url: external, resolvingAgainstBaseURL: false)?.fragment
        )
        let payload = try decodeBase64URL(
            String(fragment.dropFirst("attach=".count))
        )
        XCTAssertThrowsError(
            try PreviewLaunchConfiguration.parse(descriptorData: payload)
        ) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .nonLoopbackURL)
        }

        XCTAssertThrowsError(
            try PreviewLaunchConfiguration.parse(
                descriptorData: Data(
                    repeating: 0x61,
                    count: PreviewLaunchConfiguration.maximumEncodedPayloadBytes + 1
                )
            )
        ) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .payloadTooLarge)
        }
    }

    func testReceiverFragmentUsesBrowserContractWithoutQueryCredentials() throws {
        let configuration = try PreviewLaunchConfiguration.parse(deepLink: makeDeepLink())
        let receiverURL = try configuration.session.receiverURL(presentation: .expanded)
        let components = try XCTUnwrap(
            URLComponents(url: receiverURL, resolvingAgainstBaseURL: false)
        )

        XCTAssertEqual(components.scheme, "http")
        XCTAssertEqual(components.host, "127.0.0.1")
        XCTAssertEqual(components.port, 7_331)
        XCTAssertNil(components.query)
        XCTAssertTrue(components.fragment?.hasPrefix("attach=") == true)

        let encoded = try XCTUnwrap(components.fragment).dropFirst("attach=".count)
        let payload = try decodeBase64URL(String(encoded))
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: payload) as? [String: Any]
        )
        XCTAssertEqual(object["provider"] as? String, "webrtc")
        XCTAssertEqual(object["presentation"] as? String, "expanded")
        XCTAssertEqual(
            (object["signaling"] as? [String: Any])?["url"] as? String,
            "ws://127.0.0.1:7331/signal"
        )
        XCTAssertEqual(object["token"] as? String, "token_0123456789abcdefghijklmnopqrstuvwxyz")
    }

    func testRejectsExternalSignalingEndpoint() throws {
        let url = try makeDeepLink(overrides: [
            "signaling": ["url": "ws://remotehost.local:7331/signal"],
        ])

        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: url)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .nonLoopbackURL)
        }
    }

    func testRejectsCredentialsInQuery() throws {
        let canonical = try makeDeepLink()
        var components = try XCTUnwrap(
            URLComponents(url: canonical, resolvingAgainstBaseURL: false)
        )
        components.query = "token=do-not-send"
        let url = try XCTUnwrap(components.url)

        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: url)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .invalidDeepLink)
        }
    }

    func testRejectsUnknownTopLevelField() throws {
        let url = try makeDeepLink(overrides: ["future": true])

        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: url)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .unknownField("future"))
        }
    }

    func testRejectsUnknownSignalingField() throws {
        let url = try makeDeepLink(overrides: [
            "signaling": [
                "url": "ws://127.0.0.1:7331/signal",
                "fallback": "ws://example.com/signal",
            ],
        ])

        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: url)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .invalidPayload)
        }
    }

    func testRejectsUnsupportedProviderAndProfiles() throws {
        let providerURL = try makeDeepLink(overrides: ["provider": "unreal"])
        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: providerURL)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .invalidProvider)
        }

        let profileURL = try makeDeepLink(overrides: ["profile": "ultra"])
        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: profileURL)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .invalidProfile)
        }
    }

    func testRejectsNonCanonicalOrOversizedFragment() throws {
        let canonical = try makeDeepLink()
        let padded = try XCTUnwrap(URL(string: canonical.absoluteString + "="))
        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: padded))

        let percentEncoded = try XCTUnwrap(
            URL(string: canonical.absoluteString.replacingOccurrences(of: "attach=", with: "attach%3D"))
        )
        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: percentEncoded))

        let oversized = String(
            repeating: "a",
            count: PreviewLaunchConfiguration.maximumEncodedPayloadBytes + 1
        )
        let oversizedURL = try XCTUnwrap(
            URL(string: "wscrpt-preview://attach#attach=\(oversized)")
        )
        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: oversizedURL)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .payloadTooLarge)
        }
    }

    func testRejectsGenerationOutsideJavaScriptExactIntegerRange() throws {
        let zero = try makeDeepLink(overrides: ["generation": 0])
        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: zero)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .invalidGeneration)
        }

        let tooLarge = try makeDeepLink(overrides: [
            "generation": NSNumber(value: PreviewSessionDescriptor.maximumJavaScriptInteger + 1),
        ])
        XCTAssertThrowsError(try PreviewLaunchConfiguration.parse(deepLink: tooLarge)) { error in
            XCTAssertEqual(error as? PreviewConfigurationError, .invalidGeneration)
        }
    }

    func testStrictLoopbackURLValidation() throws {
        XCTAssertTrue(
            PreviewSessionDescriptor.isStrictLoopbackPageURL(
                try XCTUnwrap(URL(string: "http://127.0.0.1:7331/"))
            )
        )
        XCTAssertFalse(
            PreviewSessionDescriptor.isStrictLoopbackPageURL(
                try XCTUnwrap(URL(string: "http://localhost:7331/"))
            )
        )
        XCTAssertFalse(
            PreviewSessionDescriptor.isStrictLoopbackPageURL(
                try XCTUnwrap(URL(string: "https://127.0.0.1:7331/"))
            )
        )
        XCTAssertFalse(
            PreviewSessionDescriptor.isStrictLoopbackSignalingURL(
                try XCTUnwrap(URL(string: "ws://127.0.0.1:7331/other"))
            )
        )
    }

    private func makeDeepLink(overrides: [String: Any] = [:]) throws -> URL {
        var payload: [String: Any] = [
            "protocolVersion": 1,
            "sessionId": "session-019d",
            "generation": 3,
            "nonce": "nonce_0123456789abcdef",
            "token": "token_0123456789abcdefghijklmnopqrstuvwxyz",
            "signaling": ["url": "ws://127.0.0.1:7331/signal"],
            "profile": "mini",
            "provider": "webrtc",
            "presentation": "mini",
        ]
        for (key, value) in overrides {
            payload[key] = value
        }
        let data = try JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys])
        let encoded = data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        return try XCTUnwrap(URL(string: "wscrpt-preview://attach#attach=\(encoded)"))
    }

    private func decodeBase64URL(_ encoded: String) throws -> Data {
        var base64 = encoded.replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        let remainder = base64.count % 4
        if remainder != 0 {
            base64.append(String(repeating: "=", count: 4 - remainder))
        }
        return try XCTUnwrap(Data(base64Encoded: base64))
    }
}
