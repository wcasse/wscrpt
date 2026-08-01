import Foundation

protocol RemoteProfileStoring: AnyObject {
    func loadProfiles() throws -> [RemoteProfile]
    func saveProfiles(_ profiles: [RemoteProfile]) throws
}
final class UserDefaultsRemoteProfileStore: RemoteProfileStoring {
    static let maximumProfiles = 32

    private let defaults: UserDefaults
    private let key: String

    init(
        defaults: UserDefaults = .standard,
        key: String = "dev.wscrpt.remote-profiles.v1"
    ) {
        self.defaults = defaults
        self.key = key
    }

    func loadProfiles() throws -> [RemoteProfile] {
        guard let data = defaults.data(forKey: key) else { return [] }
        do {
            let profiles = try JSONDecoder().decode([RemoteProfile].self, from: data)
            guard profiles.count <= Self.maximumProfiles,
                  Set(profiles.map(\.id)).count == profiles.count
            else {
                throw RemoteProfileStoreError.invalidStore
            }
            return profiles
        } catch let error as RemoteProfileStoreError {
            throw error
        } catch {
            throw RemoteProfileStoreError.invalidStore
        }
    }

    func saveProfiles(_ profiles: [RemoteProfile]) throws {
        guard profiles.count <= Self.maximumProfiles,
              Set(profiles.map(\.id)).count == profiles.count
        else {
            throw RemoteProfileStoreError.tooManyProfiles
        }
        do {
            defaults.set(try JSONEncoder().encode(profiles), forKey: key)
        } catch {
            throw RemoteProfileStoreError.writeFailed
        }
    }
}

enum RemoteProfileStoreError: Error, Equatable, LocalizedError {
    case invalidStore
    case tooManyProfiles
    case writeFailed

    var errorDescription: String? {
        switch self {
        case .invalidStore:
            return "Saved remote profiles are damaged and were not loaded."
        case .tooManyProfiles:
            return "At most 32 remote profiles can be saved."
        case .writeFailed:
            return "Remote profiles could not be saved."
        }
    }
}
