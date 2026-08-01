import CryptoKit
import Foundation
import NIOCore
import NIOSSH

/// The stable identity used to scope an SSH host-key pin.
///
/// Pins deliberately include the port. It is common for development machines
/// to expose different SSH servers on different ports of the same host.
struct SSHHostEndpoint: Hashable, Codable, Sendable {
    let host: String
    let port: Int

    init(host: String, port: Int = 22) throws {
        let normalizedHost = host.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !normalizedHost.isEmpty,
              normalizedHost.utf8.count <= 1_024,
              normalizedHost.unicodeScalars.allSatisfy({
                  !CharacterSet.controlCharacters.contains($0)
              }),
              !normalizedHost.unicodeScalars.contains(where: CharacterSet.whitespacesAndNewlines.contains),
              (1 ... 65_535).contains(port)
        else {
            throw SSHHostKeyTrustError.invalidEndpoint
        }

        // DNS names are case insensitive. IPv4 and IPv6 literals are also
        // unaffected by lowercasing, so this gives pins one canonical key.
        self.host = normalizedHost.lowercased()
        self.port = port
    }

    var pinKey: String {
        "\(host):\(port)"
    }
}

/// A displayable, comparable SHA-256 SSH host-key fingerprint.
///
/// `digest` is the unpadded base64 form used by OpenSSH, without the
/// `SHA256:` prefix. The algorithm is retained as an additional strict check.
struct SSHHostKeyFingerprint: Hashable, Codable, Sendable, CustomStringConvertible {
    let algorithm: String
    let digest: String

    init(algorithm: String, digest: String) throws {
        let allowedAlgorithmCharacters = CharacterSet(
            charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789@._+-"
        )
        let allowedDigestCharacters = CharacterSet(
            charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/"
        )
        guard !algorithm.isEmpty,
              algorithm.utf8.count <= 128,
              algorithm.unicodeScalars.allSatisfy(allowedAlgorithmCharacters.contains),
              digest.utf8.count == 43,
              digest.unicodeScalars.allSatisfy(allowedDigestCharacters.contains),
              Data(base64Encoded: digest + "=")?.count == 32
        else {
            throw SSHHostKeyTrustError.invalidFingerprint
        }

        self.algorithm = algorithm
        self.digest = digest
    }

    init(hostKey: NIOSSHPublicKey) throws {
        let openSSH = String(openSSHPublicKey: hostKey)
        let fields = openSSH.split(separator: " ", omittingEmptySubsequences: true)
        guard fields.count == 2,
              let keyBlob = Data(base64Encoded: String(fields[1]))
        else {
            throw SSHHostKeyTrustError.invalidHostKey
        }

        let digest = Data(SHA256.hash(data: keyBlob)).base64EncodedString()
            .replacingOccurrences(of: "=", with: "")
        try self.init(algorithm: String(fields[0]), digest: digest)
    }

    var description: String {
        "SHA256:\(digest)"
    }
}

/// The material an app should show when asking whether to trust a host for the
/// first time. The private key is never exposed here.
struct SSHPresentedHostKey: Equatable, Sendable {
    let fingerprint: SSHHostKeyFingerprint
    let openSSHPublicKey: String

    init(hostKey: NIOSSHPublicKey) throws {
        fingerprint = try SSHHostKeyFingerprint(hostKey: hostKey)
        openSSHPublicKey = String(openSSHPublicKey: hostKey)
    }
}

enum SSHHostKeyTrustMode: Equatable, Sendable {
    /// Require this exact algorithm and SHA-256 fingerprint.
    case pinned(SSHHostKeyFingerprint)

    /// Confirm an unseen key once, then require the stored key on every later
    /// connection. A changed key is rejected before UI confirmation is asked.
    case trustOnFirstUse
}

enum SSHHostKeyTrustDecision: Equatable, Sendable {
    case acceptKnownKey
    case requireFirstUseConfirmation
    case rejectChangedKey(
        expected: SSHHostKeyFingerprint,
        presented: SSHHostKeyFingerprint
    )
}

/// Pure host-key policy kept separate from NIO so mismatch and first-use
/// behavior can be exhaustively unit tested.
enum SSHHostKeyPinPolicy {
    static func evaluate(
        mode: SSHHostKeyTrustMode,
        stored: SSHHostKeyFingerprint?,
        presented: SSHHostKeyFingerprint
    ) -> SSHHostKeyTrustDecision {
        switch mode {
        case let .pinned(expected):
            guard expected == presented else {
                return .rejectChangedKey(expected: expected, presented: presented)
            }
            return .acceptKnownKey

        case .trustOnFirstUse:
            guard let stored else {
                return .requireFirstUseConfirmation
            }
            guard stored == presented else {
                return .rejectChangedKey(expected: stored, presented: presented)
            }
            return .acceptKnownKey
        }
    }
}

/// Storage must implement `pinIfAbsent` atomically. If another connection pins
/// the endpoint first, the existing value is returned and is authoritative.
protocol SSHHostKeyPinStoring: AnyObject {
    func pinnedFingerprint(for endpoint: SSHHostEndpoint) throws -> SSHHostKeyFingerprint?

    @discardableResult
    func pinIfAbsent(
        _ fingerprint: SSHHostKeyFingerprint,
        for endpoint: SSHHostEndpoint
    ) throws -> SSHHostKeyFingerprint

    func removePin(for endpoint: SSHHostEndpoint) throws
}

/// Persistent app-owned storage for non-secret SSH host-key pins.
///
/// Host-key fingerprints are public material, so UserDefaults is appropriate;
/// passwords and private identity bytes belong in Keychain instead.
final class UserDefaultsSSHHostKeyPinStore: SSHHostKeyPinStoring {
    private let defaults: UserDefaults
    private let namespace: String
    private let lock = NSLock()

    init(
        defaults: UserDefaults = .standard,
        namespace: String = "dev.wscrpt.ssh-host-key"
    ) {
        self.defaults = defaults
        self.namespace = namespace
    }

    func pinnedFingerprint(for endpoint: SSHHostEndpoint) throws -> SSHHostKeyFingerprint? {
        lock.lock()
        defer { lock.unlock() }
        return try readPin(for: endpoint)
    }

    @discardableResult
    func pinIfAbsent(
        _ fingerprint: SSHHostKeyFingerprint,
        for endpoint: SSHHostEndpoint
    ) throws -> SSHHostKeyFingerprint {
        lock.lock()
        defer { lock.unlock() }

        if let existing = try readPin(for: endpoint) {
            return existing
        }

        do {
            defaults.set(try JSONEncoder().encode(fingerprint), forKey: storageKey(for: endpoint))
            return fingerprint
        } catch let error as SSHHostKeyTrustError {
            throw error
        } catch {
            throw SSHHostKeyTrustError.pinStoreFailure
        }
    }

    func removePin(for endpoint: SSHHostEndpoint) throws {
        lock.lock()
        defer { lock.unlock() }
        defaults.removeObject(forKey: storageKey(for: endpoint))
    }

    private func readPin(for endpoint: SSHHostEndpoint) throws -> SSHHostKeyFingerprint? {
        guard let data = defaults.data(forKey: storageKey(for: endpoint)) else {
            return nil
        }
        do {
            return try JSONDecoder().decode(SSHHostKeyFingerprint.self, from: data)
        } catch {
            // A damaged store must fail closed; treating it as a new host would
            // silently discard the security property of TOFU.
            throw SSHHostKeyTrustError.corruptPinStore
        }
    }

    private func storageKey(for endpoint: SSHHostEndpoint) -> String {
        let encodedEndpoint = Data(endpoint.pinKey.utf8).base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        return "\(namespace).\(encodedEndpoint)"
    }
}

/// Called only for a previously unseen endpoint. The app may present a native
/// fingerprint confirmation sheet and complete asynchronously.
typealias SSHFirstUseHostKeyConfirmation = (
    _ endpoint: SSHHostEndpoint,
    _ presentedKey: SSHPresentedHostKey,
    _ completion: @escaping (Bool) -> Void
) -> Void

/// NIOSSH host-key delegate that fails closed for every path except an exact
/// pin match or a positively confirmed first-use key.
final class StrictSSHHostKeyTrustDelegate: NIOSSHClientServerAuthenticationDelegate, @unchecked Sendable {
    private let endpoint: SSHHostEndpoint
    private let mode: SSHHostKeyTrustMode
    private let pinStore: SSHHostKeyPinStoring?
    private let confirmFirstUse: SSHFirstUseHostKeyConfirmation?

    /// Used by the connection configuration to prevent validating one network
    /// endpoint under another endpoint's pin namespace.
    var trustedEndpoint: SSHHostEndpoint {
        endpoint
    }

    init(
        endpoint: SSHHostEndpoint,
        mode: SSHHostKeyTrustMode,
        pinStore: SSHHostKeyPinStoring? = nil,
        confirmFirstUse: SSHFirstUseHostKeyConfirmation? = nil
    ) throws {
        if case .trustOnFirstUse = mode, pinStore == nil {
            throw SSHHostKeyTrustError.missingPinStore
        }
        self.endpoint = endpoint
        self.mode = mode
        self.pinStore = pinStore
        self.confirmFirstUse = confirmFirstUse
    }

    func validateHostKey(
        hostKey: NIOSSHPublicKey,
        validationCompletePromise: EventLoopPromise<Void>
    ) {
        let presented: SSHPresentedHostKey
        let stored: SSHHostKeyFingerprint?
        do {
            presented = try SSHPresentedHostKey(hostKey: hostKey)
            switch mode {
            case .pinned:
                // An explicit pin is authoritative and independent of any
                // stale or damaged TOFU entry for the same endpoint.
                stored = nil
            case .trustOnFirstUse:
                stored = try pinStore?.pinnedFingerprint(for: endpoint)
            }
        } catch {
            validationCompletePromise.fail(error)
            return
        }

        switch SSHHostKeyPinPolicy.evaluate(
            mode: mode,
            stored: stored,
            presented: presented.fingerprint
        ) {
        case .acceptKnownKey:
            validationCompletePromise.succeed(())

        case let .rejectChangedKey(expected, actual):
            validationCompletePromise.fail(
                SSHHostKeyTrustError.hostKeyMismatch(
                    expected: expected.description,
                    presented: actual.description
                )
            )

        case .requireFirstUseConfirmation:
            guard let pinStore, let confirmFirstUse else {
                validationCompletePromise.fail(
                    SSHHostKeyTrustError.firstUseConfirmationRequired(
                        endpoint: endpoint.pinKey,
                        fingerprint: presented.fingerprint.description
                    )
                )
                return
            }

            let completionGate = SingleInvocationGate()
            // NIOSSH invokes this delegate on its event-loop thread. UI
            // confirmation belongs on the main queue; EventLoopPromise may be
            // completed safely from that queue.
            DispatchQueue.main.async {
                confirmFirstUse(self.endpoint, presented) { accepted in
                    guard completionGate.claim() else { return }
                    guard accepted else {
                        validationCompletePromise.fail(SSHHostKeyTrustError.firstUseRejected)
                        return
                    }

                    do {
                        let effectivePin = try pinStore.pinIfAbsent(
                            presented.fingerprint,
                            for: self.endpoint
                        )
                        guard effectivePin == presented.fingerprint else {
                            throw SSHHostKeyTrustError.hostKeyMismatch(
                                expected: effectivePin.description,
                                presented: presented.fingerprint.description
                            )
                        }
                        validationCompletePromise.succeed(())
                    } catch {
                        validationCompletePromise.fail(error)
                    }
                }
            }
        }
    }
}

enum SSHHostKeyTrustError: Error, Equatable, LocalizedError {
    case invalidEndpoint
    case invalidFingerprint
    case invalidHostKey
    case missingPinStore
    case corruptPinStore
    case pinStoreFailure
    case firstUseConfirmationRequired(endpoint: String, fingerprint: String)
    case firstUseRejected
    case hostKeyMismatch(expected: String, presented: String)

    var errorDescription: String? {
        switch self {
        case .invalidEndpoint:
            return "The SSH host or port is invalid."
        case .invalidFingerprint:
            return "The SSH host-key fingerprint is invalid."
        case .invalidHostKey:
            return "The SSH server presented a malformed host key."
        case .missingPinStore:
            return "Trust on first use requires persistent host-key pin storage."
        case .corruptPinStore:
            return "The saved SSH host-key pin is damaged; the connection was rejected."
        case .pinStoreFailure:
            return "The SSH host-key pin could not be saved."
        case let .firstUseConfirmationRequired(endpoint, fingerprint):
            return "Confirm SSH host \(endpoint) with fingerprint \(fingerprint) before connecting."
        case .firstUseRejected:
            return "The SSH host key was not trusted."
        case let .hostKeyMismatch(expected, presented):
            return "SSH host key changed (expected \(expected), received \(presented))."
        }
    }
}

private final class SingleInvocationGate {
    private let lock = NSLock()
    private var wasClaimed = false

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !wasClaimed else { return false }
        wasClaimed = true
        return true
    }
}
