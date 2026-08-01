import Combine
import Foundation
import SwiftUI

@MainActor
final class SSHHostKeyPrompt: Identifiable {
    let id = UUID()
    let endpoint: SSHHostEndpoint
    let presentedKey: SSHPresentedHostKey

    private var completion: ((Bool) -> Void)?

    init(
        endpoint: SSHHostEndpoint,
        presentedKey: SSHPresentedHostKey,
        completion: @escaping (Bool) -> Void
    ) {
        self.endpoint = endpoint
        self.presentedKey = presentedKey
        self.completion = completion
    }

    func resolve(_ accepted: Bool) {
        let callback = completion
        completion = nil
        callback?(accepted)
    }
}

struct WorkspaceNotice: Identifiable, Equatable {
    let id = UUID()
    let title: String
    let message: String

    static func == (lhs: WorkspaceNotice, rhs: WorkspaceNotice) -> Bool {
        lhs.id == rhs.id
    }
}

/// Owns the native workspace's stateful surfaces and network lifetimes.
///
/// There is exactly one instance per scene: one SwiftTerm terminal, one
/// WKWebView/WebRTC receiver, one SSH connection, and at most one loopback
/// signaling forward. tmux remains the durable remote process boundary.
@MainActor
final class WorkspaceSessionModel: ObservableObject, TerminalSurfaceDelegate {
    let webSurface: WKWebRTCPreviewSurface
    let previewModel: PreviewSurfaceModel
    let terminalController: TerminalSurfaceController

    @Published private(set) var connectionState: WorkspaceConnectionState = .disconnected
    @Published private(set) var activeProfile: RemoteProfile?
    @Published private(set) var previewSessions: [RemotePreviewSession] = []
    @Published private(set) var previewSessionState: WorkspacePreviewSessionState = .unavailable
    @Published private(set) var devicePublicKey: String?
    @Published var hostKeyPrompt: SSHHostKeyPrompt?
    @Published var notice: WorkspaceNotice?

    private let secretStore: any SecretStoring
    private let profileStore: any RemoteProfileStoring
    private let hostKeyPinStore: any SSHHostKeyPinStoring
    private let resourceRetirementGate = WorkspaceResourceRetirementGate()

    private var transport: SSHTransport?
    private var previewCoordinator: PreviewCoordinator?
    private var connectionEpoch: UInt64 = 0
    private var previewEpoch: UInt64 = 0
    private var transportToken: UUID?
    private var terminalOutboundBuffer = TerminalOutboundBuffer()
    private var terminalDrainTask: Task<Void, Never>?
    private var terminalDrainID: UUID?
    private var cachedConnection: CachedConnection?
    private var lastAttachedPreviewSessionID: String?
    private var isForeground = true
    private var shouldReconnectWhenActive = false
    private var previewStateCancellable: AnyCancellable?

    init(
        secretStore: any SecretStoring = KeychainStore(),
        profileStore: any RemoteProfileStoring = UserDefaultsRemoteProfileStore(),
        hostKeyPinStore: any SSHHostKeyPinStoring = UserDefaultsSSHHostKeyPinStore()
    ) {
        let surface = WKWebRTCPreviewSurface()
        let terminal = TerminalSurfaceController()
        webSurface = surface
        previewModel = PreviewSurfaceModel(controller: surface)
        terminalController = terminal
        self.secretStore = secretStore
        self.profileStore = profileStore
        self.hostKeyPinStore = hostKeyPinStore

        do {
            activeProfile = try profileStore.loadProfiles().first
        } catch {
            activeProfile = nil
            notice = WorkspaceNotice(
                title: "Saved profiles unavailable",
                message: error.localizedDescription
            )
        }
        terminal.delegate = self
        previewStateCancellable = previewModel.$state
            .sink { [weak self] state in
                guard case let .failed(message) = state else { return }
                self?.handlePreviewSurfaceFailure(message)
            }
    }

    func connect(_ request: WorkspaceConnectionRequest) {
        connectionEpoch &+= 1
        cachedConnection = nil
        shouldReconnectWhenActive = false
        // An explicit connect is a new user intent. Only automatic recovery
        // may reuse the last selected session on the same cached profile;
        // carrying a bare session ID across hosts/workspaces could autoattach
        // an unrelated same-named session when several are ready.
        lastAttachedPreviewSessionID = nil
        // Disable terminal input synchronously. The prior transport is cleared
        // at the start of async teardown, so leaving `.connected` visible here
        // would silently discard keystrokes during an explicit reconnect.
        setConnectionState(.connecting)
        previewSessionState = .unavailable
        let epoch = connectionEpoch
        Task { [weak self] in
            guard let self, self.connectionEpoch == epoch else { return }
            await self.connect(request, epoch: epoch)
        }
    }

    func disconnect() {
        connectionEpoch &+= 1
        previewEpoch &+= 1
        let epoch = connectionEpoch
        cachedConnection = nil
        shouldReconnectWhenActive = false
        lastAttachedPreviewSessionID = nil
        setConnectionState(.disconnecting)
        Task { [weak self] in
            guard let self else { return }
            guard self.connectionEpoch == epoch else { return }
            await self.tearDownConnection(
                showDisconnecting: true,
                epoch: epoch
            )
            guard self.connectionEpoch == epoch else { return }
            self.setConnectionState(.disconnected)
            self.previewSessionState = .unavailable
        }
    }

    func refreshPreviewSessions() {
        guard connectionState.isConnected, let coordinator = previewCoordinator else {
            previewSessionState = .unavailable
            return
        }
        previewEpoch &+= 1
        let epoch = previewEpoch
        previewSessionState = .refreshing
        Task { [weak self] in
            guard let self else { return }
            guard self.previewEpoch == epoch,
                  self.previewCoordinator === coordinator
            else { return }
            do {
                let sessions = try await coordinator.refreshSessions()
                guard self.previewEpoch == epoch else { return }
                self.previewSessions = sessions
                if let attachedSessionID = coordinator.attachedSessionID {
                    self.previewSessionState = .attached(sessionID: attachedSessionID)
                } else {
                    self.previewSessionState = .idle
                }
            } catch {
                guard self.previewEpoch == epoch else { return }
                if let attachedSessionID = coordinator.attachedSessionID {
                    self.previewSessionState = .attached(sessionID: attachedSessionID)
                    self.notice = WorkspaceNotice(
                        title: "Preview refresh failed",
                        message: error.localizedDescription
                    )
                } else {
                    self.previewSessionState = .failed(error.localizedDescription)
                }
            }
        }
    }

    func attachPreviewSession(_ sessionID: String, presentation: PreviewPresentation) {
        guard connectionState.isConnected, let coordinator = previewCoordinator else {
            previewSessionState = .unavailable
            return
        }
        previewEpoch &+= 1
        let epoch = previewEpoch
        previewSessionState = .attaching(sessionID: sessionID)
        Task { [weak self] in
            guard let self else { return }
            guard self.previewEpoch == epoch,
                  self.previewCoordinator === coordinator
            else { return }
            do {
                let profile: PreviewQualityProfile = presentation == .expanded
                    ? .expanded
                    : .mini
                try await coordinator.attach(
                    sessionID: sessionID,
                    profile: profile,
                    presentation: presentation
                )
                guard self.previewEpoch == epoch else { return }
                self.lastAttachedPreviewSessionID = sessionID
                self.previewSessions = coordinator.sessions
                self.previewSessionState = .attached(sessionID: sessionID)
            } catch {
                guard self.previewEpoch == epoch else { return }
                if let attachedSessionID = coordinator.attachedSessionID {
                    self.previewSessionState = .attached(sessionID: attachedSessionID)
                    self.notice = WorkspaceNotice(
                        title: "Preview attach failed",
                        message: error.localizedDescription
                    )
                } else {
                    self.previewSessionState = .failed(error.localizedDescription)
                }
            }
        }
    }

    func detachPreviewSession() {
        previewEpoch &+= 1
        let epoch = previewEpoch
        lastAttachedPreviewSessionID = nil
        guard let coordinator = previewCoordinator else {
            previewModel.close()
            previewSessionState = connectionState.isConnected ? .idle : .unavailable
            return
        }
        previewSessionState = .detaching
        Task { [weak self] in
            guard let self else { return }
            guard self.previewEpoch == epoch,
                  self.previewCoordinator === coordinator
            else { return }
            await coordinator.detach()
            guard self.previewEpoch == epoch else { return }
            self.previewSessionState = self.connectionState.isConnected ? .idle : .unavailable
        }
    }

    func prepareDeviceKey(profileID: UUID) {
        do {
            let identity = try SSHEd25519Identity.loadOrCreate(
                secretStore: secretStore,
                account: SSHSecretAccount.deviceIdentity(profileID: profileID)
            )
            devicePublicKey = identity.openSSHPublicKey
        } catch {
            notice = WorkspaceNotice(
                title: "Device key unavailable",
                message: error.localizedDescription
            )
        }
    }

    func resolveHostKeyPrompt(accepted: Bool) {
        let prompt = hostKeyPrompt
        hostKeyPrompt = nil
        prompt?.resolve(accepted)
    }

    func handleScenePhase(_ phase: ScenePhase) {
        switch phase {
        case .active:
            isForeground = true
            guard shouldReconnectWhenActive, cachedConnection != nil else { return }
            shouldReconnectWhenActive = false
            connectionEpoch &+= 1
            previewEpoch &+= 1
            let epoch = connectionEpoch
            setConnectionState(.reconnecting(attempt: 1))
            Task { [weak self] in
                await self?.recoverFromUnexpectedDisconnect(
                    message: "The iPad workspace resumed.",
                    epoch: epoch
                )
            }
        case .background:
            isForeground = false
            connectionEpoch &+= 1
            previewEpoch &+= 1
            if let cachedConnection,
               case .tmux = cachedConnection.profile.launchStyle
            {
                shouldReconnectWhenActive = true
            } else {
                shouldReconnectWhenActive = false
                self.cachedConnection = nil
            }
            let epoch = connectionEpoch
            setConnectionState(.disconnecting)
            Task { [weak self] in
                guard let self else { return }
                guard self.connectionEpoch == epoch else { return }
                await self.tearDownConnection(
                    showDisconnecting: false,
                    epoch: epoch
                )
                guard self.connectionEpoch == epoch else { return }
                self.setConnectionState(.disconnected)
                self.previewSessionState = .unavailable
            }
        case .inactive:
            break
        @unknown default:
            break
        }
    }

    func terminalSurface(
        _ surface: TerminalSurfaceController,
        didSend data: ArraySlice<UInt8>
    ) {
        guard surface === terminalController,
              connectionState.acceptsTerminalInput,
              let transport
        else {
            return
        }
        do {
            try terminalOutboundBuffer.appendInput(Data(data))
            startTerminalDrain(for: transport)
        } catch {
            beginTerminalIOFailure(
                error,
                transport: transport,
                token: transportToken
            )
        }
    }

    func terminalSurface(
        _ surface: TerminalSurfaceController,
        didResize dimensions: TerminalDimensions
    ) {
        guard surface === terminalController,
              let transport,
              transport.state == .connected,
              let size = try? SSHTerminalSize(
                  columns: dimensions.columns,
                  rows: dimensions.rows,
                  pixelWidth: dimensions.pixelWidth,
                  pixelHeight: dimensions.pixelHeight
              )
        else {
            return
        }
        terminalOutboundBuffer.replacePendingResize(with: size)
        startTerminalDrain(for: transport)
    }

    private func connect(_ request: WorkspaceConnectionRequest, epoch: UInt64) async {
        guard epoch == connectionEpoch, !Task.isCancelled else { return }
        await tearDownConnection(showDisconnecting: false, epoch: epoch)
        guard epoch == connectionEpoch, !Task.isCancelled else { return }

        if let previousProfile = activeProfile,
           previousProfile.host != request.profile.host
            || previousProfile.port != request.profile.port
            || previousProfile.username != request.profile.username
            || previousProfile.workspace != request.profile.workspace
            || previousProfile.launchStyle != request.profile.launchStyle
        {
            // A stable terminal is valuable across reconnects to the same tmux
            // session, but output from another host/profile must not linger.
            terminalController.clearDisplay()
        }
        activeProfile = request.profile
        setConnectionState(.connecting)
        previewSessionState = .unavailable

        do {
            persist(request.profile)
            let credentials = try resolveCredentials(for: request)
            let candidate = CachedConnection(
                profile: request.profile,
                credentials: credentials
            )
            try await openConnection(candidate, epoch: epoch, reconnectAttempt: nil)
            guard epoch == connectionEpoch else { return }
        } catch {
            guard epoch == connectionEpoch else { return }
            cachedConnection = nil
            shouldReconnectWhenActive = false
            await tearDownConnection(showDisconnecting: false, epoch: epoch)
            guard epoch == connectionEpoch else { return }
            setConnectionState(.failed(error.localizedDescription))
            previewSessionState = .unavailable
        }
    }

    private func openConnection(
        _ cached: CachedConnection,
        epoch: UInt64,
        reconnectAttempt: Int?
    ) async throws {
        // An older coordinator can be waiting for an already-sent token
        // command to finish after cancellation. Opening a replacement SSH
        // connection before that retirement completes would let the older
        // command revoke a token minted by the replacement coordinator.
        await resourceRetirementGate.waitForIdle()
        guard epoch == connectionEpoch else { throw SSHTransportError.connectionCancelled }

        let profile = cached.profile
        let endpoint = try SSHHostEndpoint(host: profile.host, port: Int(profile.port))
        let trustDelegate = try StrictSSHHostKeyTrustDelegate(
            endpoint: endpoint,
            mode: .trustOnFirstUse,
            pinStore: hostKeyPinStore,
            confirmFirstUse: { [weak self] endpoint, key, completion in
                Task { @MainActor [weak self] in
                    guard let self, self.connectionEpoch == epoch else {
                        completion(false)
                        return
                    }
                    self.hostKeyPrompt?.resolve(false)
                    self.hostKeyPrompt = SSHHostKeyPrompt(
                        endpoint: endpoint,
                        presentedKey: key,
                        completion: completion
                    )
                    self.setConnectionState(.authenticating)
                }
            }
        )
        let configuration = try SSHConnectionConfiguration(
            endpoint: endpoint,
            username: profile.username,
            credentials: cached.credentials,
            hostKeyTrust: trustDelegate
        )

        let token = UUID()
        transportToken = token
        let callbacks = SSHTransportCallbacks(
            onOutput: { [weak self] chunk in
                // This transport is constructed with DispatchQueue.main.
                // Consume synchronously so SSHTransport's bounded output
                // accounting includes the actual SwiftTerm feed; a second
                // untracked MainActor task hop would escape that ceiling.
                MainActor.assumeIsolated {
                    guard let self, self.transportToken == token else { return }
                    self.terminalController.receive(chunk.data)
                }
            },
            onStateChange: { [weak self] state in
                MainActor.assumeIsolated {
                    self?.handleTransportState(state, token: token)
                }
            }
        )
        let newTransport = SSHTransport(callbackQueue: .main, callbacks: callbacks)
        transport = newTransport

        if let reconnectAttempt {
            setConnectionState(.reconnecting(attempt: reconnectAttempt))
        } else {
            setConnectionState(.connecting)
        }
        let dimensions = terminalController.dimensions
        let initialSize = try SSHTerminalSize(
            columns: dimensions.columns,
            rows: dimensions.rows,
            pixelWidth: dimensions.pixelWidth,
            pixelHeight: dimensions.pixelHeight
        )
        try await newTransport.connect(
            configuration: configuration,
            initialSize: initialSize
        )
        guard epoch == connectionEpoch, transportToken == token else {
            await newTransport.close()
            throw SSHTransportError.connectionCancelled
        }

        hostKeyPrompt = nil
        setConnectionState(.openingTerminal)
        // The iPad can rotate while DNS, authentication, or first-use host-key
        // confirmation is in flight. Flush the controller's latest grid after
        // PTY setup; subsequent pre-launch resizes are accepted because the
        // transport is now connected even though terminal input remains gated.
        let latestDimensions = terminalController.dimensions
        let latestSize = try SSHTerminalSize(
            columns: latestDimensions.columns,
            rows: latestDimensions.rows,
            pixelWidth: latestDimensions.pixelWidth,
            pixelHeight: latestDimensions.pixelHeight
        )
        if latestSize != initialSize {
            try await newTransport.resize(latestSize)
        }
        let launch = RemoteLaunchCommandBuilder.command(for: profile) + "\r"
        try await newTransport.send(Data(launch.utf8))
        guard epoch == connectionEpoch else {
            await newTransport.close()
            throw SSHTransportError.connectionCancelled
        }

        setConnectionState(.connected)
        // A connection becomes eligible for lifecycle reconnect only after
        // authentication, PTY setup, and the first terminal launch write have
        // all succeeded. Preview discovery is intentionally non-fatal.
        cachedConnection = cached
            await configurePreview(
                profile: profile,
                transport: newTransport,
                epoch: epoch
            )
    }

    private func configurePreview(
        profile: RemoteProfile,
        transport: SSHTransport,
        epoch: UInt64
    ) async {
        do {
            let commands = try RemotePreviewCommandBuilder(
                workspacePath: profile.workspace,
                previewToolsPath: profile.previewToolsPath
            )
            let coordinator = PreviewCoordinator(
                ssh: transport,
                commands: commands,
                previewModel: previewModel
            )
            previewCoordinator = coordinator
            previewSessionState = .refreshing

            let sessions = try await coordinator.refreshSessions()
            guard epoch == connectionEpoch else { return }
            previewSessions = sessions
            let ready = sessions.filter(\.isAttachable)
            let preferred = lastAttachedPreviewSessionID.flatMap { prior in
                ready.first { $0.id == prior }
            }
            let automatic = preferred ?? (ready.count == 1 ? ready[0] : nil)
            guard let automatic else {
                previewSessionState = .idle
                return
            }

            previewSessionState = .attaching(sessionID: automatic.id)
            let presentation = previewModel.presentation
            try await coordinator.attach(
                sessionID: automatic.id,
                profile: presentation == .expanded ? .expanded : .mini,
                presentation: presentation
            )
            guard epoch == connectionEpoch else { return }
            lastAttachedPreviewSessionID = automatic.id
            previewSessions = coordinator.sessions
            previewSessionState = .attached(sessionID: automatic.id)
        } catch {
            guard epoch == connectionEpoch else { return }
            // A missing preview daemon must not take down a healthy terminal.
            previewSessionState = .failed(error.localizedDescription)
        }
    }

    private func handleTransportState(_ state: SSHTransportState, token: UUID) {
        guard transportToken == token else { return }
        switch state {
        case .idle, .connecting, .connected, .closing, .closed:
            break
        case let .failed(message):
            // connect()/openConnection() own handshake failures. Only a loss
            // of an already usable workspace starts the bounded reconnect
            // loop; otherwise the transport callback would race the awaited
            // error path and create two reconnect owners.
            guard connectionState.isConnected else { return }
            connectionEpoch &+= 1
            previewEpoch &+= 1
            let epoch = connectionEpoch
            setConnectionState(.reconnecting(attempt: 1))
            Task { [weak self] in
                await self?.recoverFromUnexpectedDisconnect(message: message, epoch: epoch)
            }
        }
    }

    private func recoverFromUnexpectedDisconnect(message: String, epoch: UInt64) async {
        let cached = cachedConnection
        if isForeground, cached != nil {
            setConnectionState(.reconnecting(attempt: 1))
        } else {
            setConnectionState(.failed(message))
        }
        await tearDownConnection(showDisconnecting: false, epoch: epoch)
        guard epoch == connectionEpoch else { return }

        guard isForeground, let cached else {
            setConnectionState(.failed(message))
            previewSessionState = .unavailable
            return
        }

        for attempt in 1 ... 4 {
            guard epoch == connectionEpoch, isForeground else { return }
            setConnectionState(.reconnecting(attempt: attempt))
            let delaySeconds = 1 << (attempt - 1)
            try? await Task.sleep(for: .seconds(delaySeconds))
            guard epoch == connectionEpoch, isForeground else { return }
            do {
                try await openConnection(
                    cached,
                    epoch: epoch,
                    reconnectAttempt: attempt
                )
                return
            } catch {
                await tearDownConnection(showDisconnecting: false, epoch: epoch)
                guard epoch == connectionEpoch else { return }
                if attempt == 4 {
                    cachedConnection = nil
                    shouldReconnectWhenActive = false
                    setConnectionState(.failed(error.localizedDescription))
                    previewSessionState = .unavailable
                }
            }
        }
    }

    private func tearDownConnection(
        showDisconnecting: Bool,
        epoch: UInt64
    ) async {
        guard connectionEpoch == epoch else { return }
        assert(
            !connectionState.acceptsTerminalInput,
            "Connection teardown must disable terminal input before clearing transport"
        )
        let existingCoordinator = previewCoordinator
        let existingTransport = transport
        previewCoordinator = nil
        transport = nil
        transportToken = nil
        terminalDrainTask?.cancel()
        terminalDrainTask = nil
        terminalDrainID = nil
        terminalOutboundBuffer.removeAll()
        hostKeyPrompt?.resolve(false)
        hostKeyPrompt = nil

        if showDisconnecting, existingTransport != nil {
            setConnectionState(.disconnecting)
        }
        let previewModel = previewModel
        await resourceRetirementGate.enqueueAndWait {
            if let existingCoordinator {
                await existingCoordinator.detach()
            } else {
                previewModel.close()
            }
            await existingTransport?.close()
        }

        guard connectionEpoch == epoch else { return }
        previewSessions = []
        terminalController.updateConnectionState(.disconnected)
    }

    private func setConnectionState(_ state: WorkspaceConnectionState) {
        connectionState = state
        let terminalState: TerminalConnectionState
        switch state {
        case .disconnected, .disconnecting:
            terminalState = .disconnected
        case .connecting:
            terminalState = .connecting
        case .authenticating:
            terminalState = .authenticating
        case .openingTerminal:
            terminalState = .openingPTY
        case .connected:
            terminalState = .connected
        case let .reconnecting(attempt):
            terminalState = .reconnecting(attempt: attempt)
        case let .failed(message):
            terminalState = .failed(message)
        }
        terminalController.updateConnectionState(terminalState)
    }

    private func resolveCredentials(
        for request: WorkspaceConnectionRequest
    ) throws -> [SSHAuthenticationCredential] {
        switch request.authentication {
        case let .password(value, remember):
            let account = SSHSecretAccount.password(profile: request.profile)
            let legacyAccount = SSHSecretAccount.legacyPassword(
                profileID: request.profile.id
            )
            let password: String
            if let value {
                guard !value.isEmpty, value.utf8.count <= 4_096 else {
                    throw WorkspaceSessionError.invalidPassword
                }
                password = value
                if remember {
                    try secretStore.set(
                        Data(value.utf8),
                        for: account
                    )
                    try secretStore.removeData(for: legacyAccount)
                } else {
                    // "Remember in Keychain" is authoritative. Replacing a
                    // previously saved password with an ephemeral one must not
                    // leave the older credential available for a later blank
                    // password submission.
                    try secretStore.removeData(
                        for: account
                    )
                    try secretStore.removeData(for: legacyAccount)
                }
            } else {
                guard let data = try secretStore.data(
                    for: account
                ),
                    let saved = String(data: data, encoding: .utf8),
                    !saved.isEmpty,
                    saved.utf8.count <= 4_096
                else {
                    throw WorkspaceSessionError.passwordRequired
                }
                password = saved
            }
            return [.password(password)]

        case .deviceKey:
            let identity = try SSHEd25519Identity.loadOrCreate(
                secretStore: secretStore,
                account: SSHSecretAccount.deviceIdentity(profileID: request.profile.id)
            )
            devicePublicKey = identity.openSSHPublicKey
            return [.ed25519(identity)]
        }
    }

    private func persist(_ profile: RemoteProfile) {
        do {
            var profiles = try profileStore.loadProfiles()
            profiles.removeAll { $0.id == profile.id }
            profiles.insert(profile, at: 0)
            try profileStore.saveProfiles(profiles)
        } catch {
            notice = WorkspaceNotice(
                title: "Profile not saved",
                message: error.localizedDescription
            )
        }
    }

    private func startTerminalDrain(for transport: SSHTransport) {
        guard terminalDrainTask == nil,
              let token = transportToken,
              self.transport === transport
        else {
            return
        }

        let drainID = UUID()
        terminalDrainID = drainID
        terminalDrainTask = Task { @MainActor [weak self, weak transport] in
            guard let self, let transport else { return }
            await self.drainTerminalOperations(
                through: transport,
                token: token,
                drainID: drainID
            )
        }
    }

    private func drainTerminalOperations(
        through transport: SSHTransport,
        token: UUID,
        drainID: UUID
    ) async {
        defer {
            if terminalDrainID == drainID {
                terminalDrainTask = nil
                terminalDrainID = nil
            }
        }

        while !Task.isCancelled,
              transportToken == token,
              self.transport === transport,
              let operation = terminalOutboundBuffer.takeNext()
        {
            do {
                switch operation {
                case let .input(payload):
                    try await transport.send(payload)
                case let .resize(size):
                    try await transport.resize(size)
                }
            } catch {
                guard !Task.isCancelled,
                      transportToken == token,
                      self.transport === transport
                else {
                    return
                }
                await failTerminalIO(
                    error,
                    transport: transport,
                    token: token
                )
                return
            }
        }
    }

    private func beginTerminalIOFailure(
        _ error: Error,
        transport: SSHTransport,
        token: UUID?
    ) {
        guard let token else { return }
        Task { @MainActor [weak self, weak transport] in
            guard let self, let transport else { return }
            await self.failTerminalIO(
                error,
                transport: transport,
                token: token
            )
        }
    }

    private func failTerminalIO(
        _ error: Error,
        transport: SSHTransport,
        token: UUID
    ) async {
        guard transportToken == token, self.transport === transport else { return }
        setConnectionState(.failed(error.localizedDescription))
        connectionEpoch &+= 1
        previewEpoch &+= 1
        let epoch = connectionEpoch
        cachedConnection = nil
        shouldReconnectWhenActive = false
        notice = WorkspaceNotice(
            title: "Terminal I/O failed",
            message: error.localizedDescription
        )
        await tearDownConnection(showDisconnecting: false, epoch: epoch)
        guard connectionEpoch == epoch else { return }
        setConnectionState(.failed(error.localizedDescription))
        previewSessionState = .unavailable
    }

    private func handlePreviewSurfaceFailure(_ message: String) {
        guard let coordinator = previewCoordinator else { return }
        previewEpoch &+= 1
        let epoch = previewEpoch
        lastAttachedPreviewSessionID = nil
        previewSessionState = .failed(message)
        Task { @MainActor [weak self, weak coordinator] in
            guard let self, let coordinator,
                  self.previewEpoch == epoch,
                  self.previewCoordinator === coordinator
            else {
                return
            }
            await coordinator.detach()
            guard self.previewEpoch == epoch else { return }
            // Keep the terminal usable and retain the actionable receiver
            // failure even though detach resets the presenter to idle.
            self.previewSessionState = .failed(message)
        }
    }
}

/// Serializes retirement across connection epochs. Clearing the model's
/// resource pointers is synchronous so stale callbacks are ignored, but the
/// resources themselves can take time to close: an SSH exec that has already
/// begun deliberately waits for bounded completion. Every future connection
/// waits for this queue to become empty before it may authenticate or mutate
/// preview token state.
@MainActor
final class WorkspaceResourceRetirementGate {
    private struct Lease {
        let id: UUID
        let task: Task<Void, Never>
    }

    private var tail: Lease?

    func enqueueAndWait(
        _ operation: @escaping @MainActor @Sendable () async -> Void
    ) async {
        let predecessor = tail?.task
        let id = UUID()
        let task = Task { @MainActor in
            await predecessor?.value
            await operation()
        }
        tail = Lease(id: id, task: task)
        await task.value
        clearTail(ifOwnedBy: id)
    }

    func waitForIdle() async {
        while let lease = tail {
            await lease.task.value
            clearTail(ifOwnedBy: lease.id)
        }
    }

    private func clearTail(ifOwnedBy id: UUID) {
        guard tail?.id == id else { return }
        tail = nil
    }
}

enum TerminalOutboundOperation: Equatable {
    case input(Data)
    case resize(SSHTerminalSize)
}

struct TerminalOutboundBuffer {
    static let maximumPendingInputBytes = 1_048_576

    private var pendingInput = Data()
    private var pendingInputOffset = 0
    private var pendingResize: SSHTerminalSize?

    var pendingInputByteCount: Int {
        pendingInput.count - pendingInputOffset
    }

    mutating func appendInput(_ data: Data) throws {
        guard !data.isEmpty else { return }
        guard data.count <= Self.maximumPendingInputBytes - pendingInputByteCount else {
            throw WorkspaceSessionError.terminalInputBacklogExceeded
        }
        compactIfNeeded()
        pendingInput.append(data)
    }

    mutating func replacePendingResize(with size: SSHTerminalSize) {
        pendingResize = size
    }

    mutating func takeNext() -> TerminalOutboundOperation? {
        if let resize = pendingResize {
            pendingResize = nil
            return .resize(resize)
        }
        guard pendingInputByteCount > 0 else { return nil }
        let count = min(
            pendingInputByteCount,
            SSHTransport.maximumInputChunkBytes
        )
        let range = pendingInputOffset ..< pendingInputOffset + count
        let chunk = pendingInput.subdata(in: range)
        pendingInputOffset += count
        compactIfNeeded()
        return .input(chunk)
    }

    mutating func removeAll() {
        pendingInput.removeAll(keepingCapacity: false)
        pendingInputOffset = 0
        pendingResize = nil
    }

    private mutating func compactIfNeeded() {
        guard pendingInputOffset > 0 else { return }
        if pendingInputOffset == pendingInput.count {
            pendingInput.removeAll(keepingCapacity: true)
            pendingInputOffset = 0
        } else if pendingInputOffset >= 65_536,
                  pendingInputOffset >= pendingInput.count / 2
        {
            pendingInput.removeSubrange(0 ..< pendingInputOffset)
            pendingInputOffset = 0
        }
    }
}

private struct CachedConnection {
    let profile: RemoteProfile
    let credentials: [SSHAuthenticationCredential]
}

private enum WorkspaceSessionError: Error, LocalizedError {
    case invalidPassword
    case passwordRequired
    case terminalInputBacklogExceeded

    var errorDescription: String? {
        switch self {
        case .invalidPassword:
            return "SSH password must be between 1 and 4096 UTF-8 bytes."
        case .passwordRequired:
            return "Enter an SSH password or save one in Keychain for this profile."
        case .terminalInputBacklogExceeded:
            return "The SSH connection stopped accepting terminal input before the 1 MiB safety limit was reached. The session was closed without silently dropping keystrokes."
        }
    }
}
