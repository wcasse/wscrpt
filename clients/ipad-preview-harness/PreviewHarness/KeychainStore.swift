import CryptoKit
import Foundation
import Security

protocol SecretStoring: AnyObject {
    func data(for account: String) throws -> Data?
    func set(_ data: Data, for account: String) throws
    func removeData(for account: String) throws
}

/// Small Keychain wrapper for SSH passwords and app-generated identity bytes.
/// Profile metadata and host-key fingerprints are intentionally stored
/// elsewhere because neither is secret.
final class KeychainStore: SecretStoring {
    private let service: String
    private let accessGroup: String?

    init(
        service: String = "dev.wscrpt.native-terminal",
        accessGroup: String? = nil
    ) {
        self.service = service
        self.accessGroup = accessGroup
    }

    func data(for account: String) throws -> Data? {
        let itemQuery = baseQuery(account: account)
        var query = itemQuery
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        query[kSecReturnData as String] = true

        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        switch status {
        case errSecSuccess:
            guard let data = item as? Data else {
                throw KeychainStoreError.unexpectedItem
            }
            // Existing development installs may have records created with
            // AfterFirstUnlock. A successful foreground read upgrades the
            // protection class in place instead of leaving legacy secrets
            // readable while the device is locked.
            let migrationStatus = SecItemUpdate(
                itemQuery as CFDictionary,
                [
                    kSecAttrAccessible as String:
                        kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                ] as CFDictionary
            )
            guard migrationStatus == errSecSuccess else {
                throw KeychainStoreError.operationFailed(migrationStatus)
            }
            return data
        case errSecItemNotFound:
            return nil
        default:
            throw KeychainStoreError.operationFailed(status)
        }
    }

    func set(_ data: Data, for account: String) throws {
        guard !data.isEmpty else {
            throw KeychainStoreError.emptySecret
        }

        let query = baseQuery(account: account)
        let attributes: [String: Any] = [
            kSecValueData as String: data,
            // The app tears network state down outside the foreground and has
            // no background credential use. Keep secrets device-bound and
            // unavailable whenever the iPad itself is locked.
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        let updateStatus = SecItemUpdate(
            query as CFDictionary,
            attributes as CFDictionary
        )
        if updateStatus == errSecSuccess {
            return
        }
        guard updateStatus == errSecItemNotFound else {
            throw KeychainStoreError.operationFailed(updateStatus)
        }

        var insert = query
        attributes.forEach { insert[$0.key] = $0.value }
        let insertStatus = SecItemAdd(insert as CFDictionary, nil)
        guard insertStatus == errSecSuccess else {
            throw KeychainStoreError.operationFailed(insertStatus)
        }
    }

    func removeData(for account: String) throws {
        let status = SecItemDelete(baseQuery(account: account) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainStoreError.operationFailed(status)
        }
    }

    private func baseQuery(account: String) -> [String: Any] {
        var query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecUseDataProtectionKeychain as String: true,
        ]
        if let accessGroup {
            query[kSecAttrAccessGroup as String] = accessGroup
        }
        return query
    }
}

enum SSHSecretAccount {
    /// Passwords are bound to both the stable profile and the exact SSH
    /// endpoint identity. Editing a saved profile to point at another host,
    /// port, or user must never make the old endpoint's password eligible for
    /// the new connection.
    static func password(profile: RemoteProfile) -> String {
        let endpointIdentity = [
            profile.username,
            profile.host,
            String(profile.port),
        ].joined(separator: "\u{0}")
        let digest = SHA256.hash(data: Data(endpointIdentity.utf8))
            .map { String(format: "%02x", $0) }
            .joined()
        return "ssh-password.v2.\(profile.id.uuidString.lowercased()).\(digest)"
    }

    /// The original Phase 0 account was scoped only by profile UUID. It is
    /// removed opportunistically, but never read, because it cannot prove
    /// which endpoint it belongs to after a profile edit.
    static func legacyPassword(profileID: UUID) -> String {
        "ssh-password.\(profileID.uuidString.lowercased())"
    }

    static func deviceIdentity(profileID: UUID) -> String {
        "ssh-ed25519.\(profileID.uuidString.lowercased())"
    }
}

enum KeychainStoreError: Error, Equatable, LocalizedError {
    case emptySecret
    case unexpectedItem
    case operationFailed(OSStatus)

    var errorDescription: String? {
        switch self {
        case .emptySecret:
            return "An empty SSH secret cannot be saved."
        case .unexpectedItem:
            return "The saved SSH credential has an unexpected format."
        case let .operationFailed(status):
            let message = SecCopyErrorMessageString(status, nil) as String?
            return message.map { "Keychain error: \($0)" }
                ?? "Keychain operation failed (\(status))."
        }
    }
}
