import Foundation
import GameController

enum PhysicalKeyboardAvailability: Equatable, Sendable {
    case present
    case absent
    case unknown
}

struct PhysicalKeyboardLaunchGate: Equatable, Sendable {
    let availability: PhysicalKeyboardAvailability
    let softwareKeyboardAcknowledged: Bool

    var permitsConnection: Bool {
        availability == .present || softwareKeyboardAcknowledged
    }

    var requiresAcknowledgement: Bool {
        !permitsConnection
    }
}

/// Tracks the system's coalesced physical keyboard without tying an SSH or
/// preview lifetime to the accessory lifetime. iOS exposes whether a physical
/// keyboard is available, not whether that keyboard is a particular model.
@MainActor
final class PhysicalKeyboardMonitor: ObservableObject {
    @Published private(set) var availability: PhysicalKeyboardAvailability

    private let notificationCenter: NotificationCenter
    private let keyboardProvider: () -> GCKeyboard?
    private var observers: [NSObjectProtocol] = []

    init(
        notificationCenter: NotificationCenter = .default,
        keyboardProvider: @escaping () -> GCKeyboard? = { GCKeyboard.coalesced }
    ) {
        self.notificationCenter = notificationCenter
        self.keyboardProvider = keyboardProvider
#if targetEnvironment(simulator)
        availability = .unknown
#else
        availability = keyboardProvider() == nil ? .absent : .present
#endif

        observers = [
            notificationCenter.addObserver(
                forName: .GCKeyboardDidConnect,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                Task { @MainActor in self?.refresh() }
            },
            notificationCenter.addObserver(
                forName: .GCKeyboardDidDisconnect,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                Task { @MainActor in self?.refresh() }
            },
        ]
    }

    deinit {
        for observer in observers {
            notificationCenter.removeObserver(observer)
        }
    }

    func refresh() {
#if targetEnvironment(simulator)
        availability = .unknown
#else
        availability = keyboardProvider() == nil ? .absent : .present
#endif
    }
}
