import SwiftUI
import UIKit

enum WorkspaceConnectionState: Equatable, Sendable {
    case disconnected
    case connecting
    case authenticating
    case openingTerminal
    case connected
    case reconnecting(attempt: Int)
    case disconnecting
    case failed(String)

    var isConnected: Bool {
        if case .connected = self { return true }
        return false
    }

    var isBusy: Bool {
        switch self {
        case .connecting, .authenticating, .openingTerminal, .reconnecting, .disconnecting:
            return true
        case .disconnected, .connected, .failed:
            return false
        }
    }

    var acceptsTerminalInput: Bool {
        isConnected
    }

    fileprivate var label: String {
        switch self {
        case .disconnected:
            return "Offline"
        case .connecting:
            return "Connecting"
        case .authenticating:
            return "Authenticating"
        case .openingTerminal:
            return "Opening PTY"
        case .connected:
            return "SSH connected"
        case let .reconnecting(attempt):
            return "Reconnect \(attempt)"
        case .disconnecting:
            return "Disconnecting"
        case .failed:
            return "Connection failed"
        }
    }

    fileprivate var symbol: String {
        switch self {
        case .connected:
            return "checkmark.circle.fill"
        case .connecting, .authenticating, .openingTerminal, .reconnecting, .disconnecting:
            return "arrow.triangle.2.circlepath"
        case .failed:
            return "exclamationmark.triangle.fill"
        case .disconnected:
            return "circle"
        }
    }

    fileprivate var color: Color {
        switch self {
        case .connected:
            return .green
        case .connecting, .authenticating, .openingTerminal, .reconnecting, .disconnecting:
            return .orange
        case .failed:
            return .red
        case .disconnected:
            return .secondary
        }
    }

    fileprivate var errorMessage: String? {
        guard case let .failed(message) = self else { return nil }
        return message
    }
}

/// Display-only projection of the app session's preview coordinator. Keeping
/// discovery and tunnel ownership out of the view makes refresh/attach/detach
/// testable without giving SwiftUI responsibility for network lifetimes.
enum WorkspacePreviewSessionState: Equatable, Sendable {
    case unavailable
    case idle
    case refreshing
    case attaching(sessionID: String)
    case attached(sessionID: String)
    case detaching
    case failed(String)

    var isBusy: Bool {
        switch self {
        case .refreshing, .attaching, .detaching:
            return true
        case .unavailable, .idle, .attached, .failed:
            return false
        }
    }

    var attachedSessionID: String? {
        guard case let .attached(sessionID) = self else { return nil }
        return sessionID
    }

    fileprivate var label: String {
        switch self {
        case .unavailable:
            return "Preview unavailable"
        case .idle:
            return "Preview idle"
        case .refreshing:
            return "Finding previews"
        case .attaching:
            return "Attaching preview"
        case .attached:
            return "Preview attached"
        case .detaching:
            return "Detaching preview"
        case .failed:
            return "Preview failed"
        }
    }

    fileprivate var errorMessage: String? {
        guard case let .failed(message) = self else { return nil }
        return message
    }
}

/// Native iPad workspace hosting exactly one terminal view and one WebKit
/// preview view. The adaptive `Layout` changes only the two panes' frames, so
/// rotation and mini/expanded transitions never rebuild or reparent either
/// stateful UIKit surface.
@MainActor
struct CombinedWorkspaceView: View {
    @ObservedObject var terminalController: TerminalSurfaceController
    @ObservedObject var previewModel: PreviewSurfaceModel

    let webSurface: WKWebRTCPreviewSurface
    let connectionState: WorkspaceConnectionState
    let activeProfile: RemoteProfile?
    let previewSessions: [RemotePreviewSession]
    let previewSessionState: WorkspacePreviewSessionState
    let devicePublicKey: String?
    let physicalKeyboardAvailability: PhysicalKeyboardAvailability
    let onConnect: (WorkspaceConnectionRequest) -> Void
    let onDisconnect: () -> Void
    let onRefreshPreviewSessions: () -> Void
    let onAttachPreviewSession: (String, PreviewPresentation) -> Void
    let onDetachPreviewSession: () -> Void
    let onPrepareDeviceKey: (UUID) -> Void

    @State private var profileDraft: RemoteProfileDraft
    @State private var password = ""
    @State private var rememberPassword = true
    @State private var profileError: String?
    @State private var isShowingConnectionSheet = false
    @State private var isShowingPreviewSessionSheet = false
    @State private var selectedPreviewSessionID: String?
    @State private var terminalFocusRequest: UInt64 = 0
    @State private var softwareKeyboardAcknowledged = false

    init(
        terminalController: TerminalSurfaceController,
        previewModel: PreviewSurfaceModel,
        webSurface: WKWebRTCPreviewSurface,
        connectionState: WorkspaceConnectionState,
        activeProfile: RemoteProfile? = nil,
        initialProfile: RemoteProfileDraft = RemoteProfileDraft(),
        previewSessions: [RemotePreviewSession] = [],
        previewSessionState: WorkspacePreviewSessionState = .idle,
        devicePublicKey: String? = nil,
        physicalKeyboardAvailability: PhysicalKeyboardAvailability = .unknown,
        onConnect: @escaping (WorkspaceConnectionRequest) -> Void,
        onDisconnect: @escaping () -> Void,
        onRefreshPreviewSessions: @escaping () -> Void = {},
        onAttachPreviewSession: @escaping (String, PreviewPresentation) -> Void = { _, _ in },
        onDetachPreviewSession: @escaping () -> Void = {},
        onPrepareDeviceKey: @escaping (UUID) -> Void = { _ in }
    ) {
        self.terminalController = terminalController
        self.previewModel = previewModel
        self.webSurface = webSurface
        self.connectionState = connectionState
        self.activeProfile = activeProfile
        self.previewSessions = previewSessions
        self.previewSessionState = previewSessionState
        self.devicePublicKey = devicePublicKey
        self.physicalKeyboardAvailability = physicalKeyboardAvailability
        self.onConnect = onConnect
        self.onDisconnect = onDisconnect
        self.onRefreshPreviewSessions = onRefreshPreviewSessions
        self.onAttachPreviewSession = onAttachPreviewSession
        self.onDetachPreviewSession = onDetachPreviewSession
        self.onPrepareDeviceKey = onPrepareDeviceKey
        _profileDraft = State(
            initialValue: activeProfile.map(RemoteProfileDraft.init(profile:)) ?? initialProfile
        )
    }

    var body: some View {
        VStack(spacing: 0) {
            workspaceToolbar
            Divider()

            if connectionState.isConnected,
               physicalKeyboardAvailability != .present
            {
                physicalKeyboardWarningBanner
            }

            AdaptiveWorkspaceLayout(
                playerExpanded: previewModel.presentation == .expanded,
                spacing: 12
            ) {
                terminalPane
                previewPane
            }
            .padding(12)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(uiColor: .systemGroupedBackground))
        .sheet(isPresented: $isShowingConnectionSheet) {
            connectionSheet
        }
        .sheet(isPresented: $isShowingPreviewSessionSheet) {
            previewSessionSheet
        }
        .onChange(of: activeProfile) { _, profile in
            guard let profile, !isShowingConnectionSheet else { return }
            profileDraft = RemoteProfileDraft(profile: profile)
        }
        .onChange(of: profileDraft) { _, _ in
            profileError = nil
        }
        .onChange(of: password) { _, _ in
            profileError = nil
        }
        .onChange(of: physicalKeyboardAvailability) { _, availability in
            if availability == .present {
                softwareKeyboardAcknowledged = false
            }
        }
        .onChange(of: connectionState) { oldState, newState in
            guard !oldState.isConnected, newState.isConnected else { return }
            terminalFocusRequest &+= 1
        }
        .onChange(of: previewSessionState) { _, newState in
            switch newState {
            case let .attached(sessionID):
                selectedPreviewSessionID = sessionID
            case .idle, .unavailable:
                selectedPreviewSessionID = nil
            case .refreshing, .attaching, .detaching, .failed:
                break
            }
        }
    }

    private var workspaceToolbar: some View {
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text(activeProfile?.name ?? "wscrpt")
                    .font(.headline)
                    .lineLimit(1)
                Text(activeProfile?.connectionDescription ?? "Native remote workspace")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer(minLength: 8)

            StatusPill(
                label: connectionState.label,
                systemImage: connectionState.symbol,
                color: connectionState.color
            )

            Button {
                preparePreviewSessionSheet()
            } label: {
                StatusPill(
                    label: previewStatusLabel,
                    systemImage: previewStatusSymbol,
                    color: previewStatusColor
                )
            }
            .buttonStyle(.plain)
            .keyboardShortcut("v", modifiers: [.command, .option])
            .help("Choose gameplay preview (Command-Option-V)")

            Button {
                terminalFocusRequest &+= 1
            } label: {
                Label("Focus terminal", systemImage: "keyboard")
                    .labelStyle(.iconOnly)
            }
            .disabled(!connectionState.acceptsTerminalInput)
            .keyboardShortcut("t", modifiers: [.command, .option])
            .help("Focus terminal (Command-Option-T)")

            Button {
                togglePreviewPresentation()
            } label: {
                Label(
                    previewModel.presentation == .expanded ? "Shrink player" : "Expand player",
                    systemImage: previewModel.presentation == .expanded
                        ? "arrow.down.right.and.arrow.up.left"
                        : "arrow.up.left.and.arrow.down.right"
                )
                .labelStyle(.iconOnly)
            }
            .keyboardShortcut("p", modifiers: [.command, .option])
            .help("Toggle player size (Command-Option-P)")

            connectionAction

            Button {
                prepareConnectionSheet()
            } label: {
                Label("Connection settings", systemImage: "slider.horizontal.3")
                    .labelStyle(.iconOnly)
            }
            .keyboardShortcut(",", modifiers: .command)
            .help("Connection settings (Command-comma)")
        }
        .buttonStyle(.borderless)
        .padding(.horizontal, 14)
        .padding(.vertical, 9)
        .background(Color(uiColor: .secondarySystemBackground))
    }

    @ViewBuilder
    private var connectionAction: some View {
        if connectionState.isConnected || connectionState.isBusy {
            Button(role: .destructive) {
                onDisconnect()
            } label: {
                Label("Disconnect", systemImage: "bolt.slash")
                    .labelStyle(.iconOnly)
            }
            .disabled(connectionState == .disconnecting)
            .help("Disconnect SSH")
        } else {
            Button {
                prepareConnectionSheet()
            } label: {
                Label("Connect", systemImage: "bolt.fill")
                    .labelStyle(.iconOnly)
            }
            .help("Connect SSH")
        }
    }

    private var terminalPane: some View {
        ZStack {
            Color(uiColor: .systemBackground)

            TerminalSurface(
                controller: terminalController,
                focusRequest: terminalFocusRequest,
                isInteractive: connectionState.acceptsTerminalInput
            )

            terminalStateOverlay
        }
        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(Color(uiColor: .separator).opacity(0.55), lineWidth: 1)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Remote terminal")
    }

    @ViewBuilder
    private var terminalStateOverlay: some View {
        switch connectionState {
        case .connected:
            EmptyView()

        case .connecting, .authenticating, .openingTerminal, .reconnecting, .disconnecting:
            VStack(spacing: 10) {
                ProgressView()
                Text(connectionState.label)
                    .font(.callout.weight(.medium))
                if let profile = activeProfile {
                    Text(profile.connectionDescription)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                }
            }
            .padding(18)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))

        case .disconnected:
            EmptyTerminalState(
                title: "Connect to a development host",
                message: "Open a real SSH PTY, attach tmux, and launch wscrpt without leaving this window.",
                buttonTitle: "Connect",
                systemImage: "terminal"
            ) {
                prepareConnectionSheet()
            }

        case let .failed(message):
            EmptyTerminalState(
                title: "SSH connection failed",
                message: message,
                buttonTitle: "Review connection",
                systemImage: "exclamationmark.triangle"
            ) {
                prepareConnectionSheet()
            }
        }
    }

    private var previewPane: some View {
        ZStack(alignment: .bottom) {
            PreviewSurface(model: previewModel, webSurface: webSurface)
                .allowsHitTesting(false)

            if let previewErrorMessage {
                PreviewErrorBanner(message: previewErrorMessage) {
                    preparePreviewSessionSheet()
                }
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(Color(uiColor: .separator).opacity(0.55), lineWidth: 1)
        }
        .shadow(
            color: .black.opacity(previewModel.presentation == .mini ? 0.32 : 0),
            radius: 18,
            y: 8
        )
    }

    private var connectionSheet: some View {
        NavigationStack {
            Form {
                Section("Keyboard preflight") {
                    Label(
                        physicalKeyboardStatusTitle,
                        systemImage: physicalKeyboardStatusSymbol
                    )
                    .foregroundStyle(physicalKeyboardStatusColor)

                    Text(physicalKeyboardStatusMessage)
                        .font(.footnote)
                        .foregroundStyle(.secondary)

                    if physicalKeyboardAvailability != .present {
                        Toggle(
                            "Continue with software keyboard (limited)",
                            isOn: $softwareKeyboardAcknowledged
                        )
                    }
                }

                Section("SSH host") {
                    TextField("Profile name (optional)", text: $profileDraft.name)

                    TextField("Host or IP address", text: $profileDraft.host)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .textContentType(.URL)

                    TextField("Port", text: $profileDraft.port)
                        .keyboardType(.numberPad)

                    TextField("Username", text: $profileDraft.username)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .textContentType(.username)
                }

                Section {
                    TextField("Remote path", text: $profileDraft.workspace)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()

                    TextField(
                        "Preview tools path (wscrpt checkout)",
                        text: $profileDraft.previewToolsPath
                    )
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()

                    Toggle("Keep wscrpt in tmux", isOn: $profileDraft.usesTmux)

                    if profileDraft.usesTmux {
                        TextField("tmux session", text: $profileDraft.tmuxSession)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                    }
                } header: {
                    Text("Workspace")
                } footer: {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(
                            "Preview tools path points to the checkout containing previewd. Use “.” only when previewd lives inside the workspace."
                        )
                        Text(
                            profileDraft.usesTmux
                                ? "Connect runs tmux new-session -A, preserving the editor across SSH reconnects."
                                : "Connect launches wscrpt directly in the SSH PTY."
                        )
                    }
                }

                Section("Authentication") {
                    Picker("Method", selection: $profileDraft.authenticationMethod) {
                        ForEach(RemoteAuthenticationMethod.allCases) { method in
                            Text(method.title).tag(method)
                        }
                    }

                    if profileDraft.authenticationMethod == .password {
                        SecureField("Password (blank uses saved password)", text: $password)
                            .textContentType(.password)
                            .submitLabel(.go)
                            .onSubmit(submitConnection)

                        Toggle("Remember in Keychain", isOn: $rememberPassword)
                            .disabled(password.isEmpty)
                    } else {
                        Label(
                            "Uses this app’s Keychain-backed Ed25519 identity. Add its public key to the host before connecting.",
                            systemImage: "key"
                        )
                        .font(.footnote)
                        .foregroundStyle(.secondary)

                        if let devicePublicKey {
                            Text(devicePublicKey)
                                .font(.caption.monospaced())
                                .textSelection(.enabled)

                            Button {
                                UIPasteboard.general.string = devicePublicKey
                            } label: {
                                Label("Copy public key", systemImage: "doc.on.doc")
                            }
                        } else {
                            Button {
                                onPrepareDeviceKey(profileDraft.id)
                            } label: {
                                Label("Generate device key", systemImage: "key.fill")
                            }
                        }
                    }
                }

                if let validationMessage {
                    Section {
                        Label(validationMessage, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.red)
                    }
                }
            }
            .navigationTitle("Remote Workspace")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") {
                        password = ""
                        profileError = nil
                        isShowingConnectionSheet = false
                    }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(connectionState.isConnected ? "Reconnect" : "Connect") {
                        submitConnection()
                    }
                    .disabled(
                        validationMessage != nil
                            || keyboardLaunchGate.requiresAcknowledgement
                    )
                }
            }
        }
        .presentationDetents([.large])
    }

    private var previewSessionSheet: some View {
        NavigationStack {
            List {
                Section {
                    previewCoordinatorStatus
                }

                Section("Gameplay previews") {
                    if previewSessions.isEmpty {
                        ContentUnavailableView {
                            Label("No live previews", systemImage: "play.rectangle.on.rectangle")
                        } description: {
                            Text(
                                connectionState.isConnected
                                    ? "Start or register a browser gameplay session on the development host, then refresh."
                                    : "Connect SSH before discovering gameplay sessions."
                            )
                        } actions: {
                            Button("Refresh") {
                                onRefreshPreviewSessions()
                            }
                            .disabled(!connectionState.isConnected || previewSessionState.isBusy)
                        }
                    } else {
                        ForEach(previewSessions) { session in
                            Button {
                                attachPreview(session)
                            } label: {
                                PreviewSessionRow(
                                    session: session,
                                    isSelected: selectedPreviewSessionID == session.id
                                )
                            }
                            .buttonStyle(.plain)
                            .disabled(!session.isAttachable || previewSessionState.isBusy)
                            .accessibilityHint(
                                session.isAttachable
                                    ? "Attaches this browser gameplay preview"
                                    : "This preview is not ready"
                            )
                        }
                    }
                }

                if let attachedSessionID = previewSessionState.attachedSessionID {
                    Section {
                        Button(role: .destructive) {
                            onDetachPreviewSession()
                        } label: {
                            Label(
                                "Detach \(attachedSessionID)",
                                systemImage: "rectangle.slash"
                            )
                        }
                        .disabled(previewSessionState.isBusy)
                    }
                }
            }
            .navigationTitle("Gameplay Preview")
            .navigationBarTitleDisplayMode(.inline)
            .refreshable {
                guard connectionState.isConnected, !previewSessionState.isBusy else {
                    return
                }
                onRefreshPreviewSessions()
            }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") {
                        isShowingPreviewSessionSheet = false
                    }
                }
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        onRefreshPreviewSessions()
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                    .disabled(!connectionState.isConnected || previewSessionState.isBusy)
                }
            }
        }
        .presentationDetents([.medium, .large])
    }

    @ViewBuilder
    private var previewCoordinatorStatus: some View {
        switch previewSessionState {
        case .refreshing, .attaching, .detaching:
            HStack(spacing: 10) {
                ProgressView()
                Text(previewSessionState.label)
            }

        case let .attached(sessionID):
            Label("Attached to \(sessionID)", systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)

        case let .failed(message):
            Label(message, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.red)

        case .unavailable:
            Label("Connect SSH to discover previews", systemImage: "bolt.slash")
                .foregroundStyle(.secondary)

        case .idle:
            Label("Choose one ready session", systemImage: "play.rectangle")
                .foregroundStyle(.secondary)
        }
    }

    private var validationMessage: String? {
        if let profileError {
            return profileError
        }
        do {
            _ = try profileDraft.validatedProfile()
        } catch {
            return error.localizedDescription
        }
        if password.utf8.count > 4_096 {
            return "SSH password is too large."
        }
        return nil
    }

    private func prepareConnectionSheet() {
        if let activeProfile {
            profileDraft = RemoteProfileDraft(profile: activeProfile)
        }
        profileError = nil
        password = ""
        softwareKeyboardAcknowledged = false
        isShowingConnectionSheet = true
    }

    private func preparePreviewSessionSheet() {
        selectedPreviewSessionID = previewSessionState.attachedSessionID
        isShowingPreviewSessionSheet = true
        if connectionState.isConnected,
           previewSessions.isEmpty,
           !previewSessionState.isBusy
        {
            onRefreshPreviewSessions()
        }
    }

    private func attachPreview(_ session: RemotePreviewSession) {
        guard session.isAttachable, !previewSessionState.isBusy else { return }
        selectedPreviewSessionID = session.id
        onAttachPreviewSession(session.id, previewModel.presentation)
    }

    private func submitConnection() {
        do {
            guard keyboardLaunchGate.permitsConnection else {
                throw WorkspaceConnectionInputError.physicalKeyboardAcknowledgementRequired
            }
            let profile = try profileDraft.validatedProfile()
            guard password.utf8.count <= 4_096 else {
                throw WorkspaceConnectionInputError.passwordTooLarge
            }

            let authentication: WorkspaceAuthenticationRequest
            switch profile.authenticationMethod {
            case .password:
                authentication = .password(
                    value: password.isEmpty ? nil : password,
                    remember: !password.isEmpty && rememberPassword
                )
            case .deviceKey:
                authentication = .deviceKey
            }

            profileError = nil
            password = ""
            profileDraft = RemoteProfileDraft(profile: profile)
            isShowingConnectionSheet = false
            onConnect(
                WorkspaceConnectionRequest(
                    profile: profile,
                    authentication: authentication
                )
            )
        } catch {
            profileError = error.localizedDescription
        }
    }

    private func togglePreviewPresentation() {
        previewModel.setPresentation(
            previewModel.presentation == .expanded ? .mini : .expanded
        )
    }

    private var keyboardLaunchGate: PhysicalKeyboardLaunchGate {
        PhysicalKeyboardLaunchGate(
            availability: physicalKeyboardAvailability,
            softwareKeyboardAcknowledged: softwareKeyboardAcknowledged
        )
    }

    private var physicalKeyboardStatusTitle: String {
        switch physicalKeyboardAvailability {
        case .present:
            return "Physical keyboard detected"
        case .absent:
            return "No physical keyboard detected"
        case .unknown:
            return "Physical keyboard status unavailable"
        }
    }

    private var physicalKeyboardStatusSymbol: String {
        physicalKeyboardAvailability == .present
            ? "keyboard.fill"
            : "keyboard.badge.ellipsis"
    }

    private var physicalKeyboardStatusColor: Color {
        physicalKeyboardAvailability == .present ? .green : .orange
    }

    private var physicalKeyboardStatusMessage: String {
        switch physicalKeyboardAvailability {
        case .present:
            return "A hardware keyboard is ready for the terminal."
        case .absent:
            return "Connect a Magic Keyboard or Bluetooth keyboard for the intended wscrpt experience, or explicitly continue in limited software-keyboard mode."
        case .unknown:
            return "This environment cannot confirm a hardware keyboard. Connect one for normal use, or explicitly continue in limited software-keyboard mode."
        }
    }

    private var physicalKeyboardWarningBanner: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: "keyboard.badge.exclamationmark")
                .foregroundStyle(.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text("Physical keyboard unavailable")
                    .font(.callout.weight(.semibold))
                Text("The remote session is still running. Reconnect the keyboard, then focus the terminal again.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .background(Color.orange.opacity(0.12))
        .accessibilityElement(children: .combine)
    }

    private var previewStatusLabel: String {
        switch previewSessionState {
        case .unavailable, .refreshing, .attaching, .detaching, .failed:
            return previewSessionState.label
        case .idle, .attached:
            break
        }

        switch previewModel.state {
        case .idle:
            return "Preview idle"
        case .connecting:
            return "Preview connecting"
        case .failed:
            return "Preview failed"
        case .playing:
            if let fps = previewModel.metrics?.presentedFPS {
                return String(format: "Preview %.0f fps", fps)
            }
            return "Preview live"
        }
    }

    private var previewStatusSymbol: String {
        switch previewSessionState {
        case .unavailable:
            return "bolt.slash"
        case .refreshing, .attaching, .detaching:
            return "arrow.triangle.2.circlepath"
        case .failed:
            return "exclamationmark.triangle.fill"
        case .idle, .attached:
            break
        }

        switch previewModel.state {
        case .idle:
            return "play.rectangle"
        case .connecting:
            return "arrow.triangle.2.circlepath"
        case .playing:
            return "play.rectangle.fill"
        case .failed:
            return "exclamationmark.triangle.fill"
        }
    }

    private var previewStatusColor: Color {
        switch previewSessionState {
        case .unavailable:
            return .secondary
        case .refreshing, .attaching, .detaching:
            return .orange
        case .failed:
            return .red
        case .idle, .attached:
            break
        }

        switch previewModel.state {
        case .idle:
            return .secondary
        case .connecting:
            return .orange
        case .playing:
            return .green
        case .failed:
            return .red
        }
    }

    private var previewErrorMessage: String? {
        if let error = previewSessionState.errorMessage {
            return error
        }
        guard case let .failed(message) = previewModel.state else { return nil }
        return message
    }
}

private enum WorkspaceConnectionInputError: Error, LocalizedError {
    case passwordTooLarge
    case physicalKeyboardAcknowledgementRequired

    var errorDescription: String? {
        switch self {
        case .passwordTooLarge:
            return "SSH password is too large."
        case .physicalKeyboardAcknowledgementRequired:
            return "Connect a physical keyboard or acknowledge limited software-keyboard mode before connecting."
        }
    }
}

private struct StatusPill: View {
    let label: String
    let systemImage: String
    let color: Color

    var body: some View {
        Label(label, systemImage: systemImage)
            .font(.caption.weight(.medium))
            .foregroundStyle(color)
            .lineLimit(1)
            .padding(.horizontal, 9)
            .padding(.vertical, 5)
            .background(color.opacity(0.12), in: Capsule())
            .accessibilityLabel(label)
    }
}

private struct EmptyTerminalState: View {
    let title: String
    let message: String
    let buttonTitle: String
    let systemImage: String
    let action: () -> Void

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: systemImage)
                .font(.system(size: 30, weight: .medium))
                .foregroundStyle(.secondary)
            Text(title)
                .font(.headline)
            Text(message)
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .lineLimit(4)
            Button(buttonTitle, action: action)
                .buttonStyle(.borderedProminent)
        }
        .padding(24)
        .frame(maxWidth: 440)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
        .padding()
    }
}

private struct PreviewSessionRow: View {
    let session: RemotePreviewSession
    let isSelected: Bool

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: session.isAttachable ? "play.rectangle.fill" : "play.rectangle")
                .font(.title3)
                .foregroundStyle(session.isAttachable ? Color.accentColor : Color.secondary)

            VStack(alignment: .leading, spacing: 3) {
                Text(session.id)
                    .font(.body.monospaced())
                    .lineLimit(1)
                HStack(spacing: 8) {
                    Text(session.state.capitalized)
                    if let width = session.sourceWidth, let height = session.sourceHeight {
                        Text("\(width)×\(height)")
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }

            Spacer()

            if isSelected {
                Image(systemName: "checkmark.circle.fill")
                    .foregroundStyle(.green)
                    .accessibilityLabel("Selected")
            } else if !session.isAttachable {
                Text("Not ready")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .contentShape(Rectangle())
        .padding(.vertical, 4)
    }
}

private struct PreviewErrorBanner: View {
    let message: String
    let action: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "exclamationmark.triangle.fill")
            Text(message)
                .font(.caption)
                .lineLimit(2)
            Spacer(minLength: 4)
            Button("Sessions", action: action)
                .buttonStyle(.bordered)
        }
        .foregroundStyle(.white)
        .padding(10)
        .background(.red.opacity(0.88), in: RoundedRectangle(cornerRadius: 9))
        .padding(10)
    }
}

/// A single layout tree is crucial here. `AnyLayout` plus conditional view
/// branches can still produce overlapping representable lifetimes during a
/// transition; this layout keeps both UIKit-backed children in place and only
/// updates their proposals and positions.
private struct AdaptiveWorkspaceLayout: Layout {
    let playerExpanded: Bool
    let spacing: CGFloat

    func sizeThatFits(
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) -> CGSize {
        proposal.replacingUnspecifiedDimensions(
            by: CGSize(width: 1_024, height: 768)
        )
    }

    func placeSubviews(
        in bounds: CGRect,
        proposal: ProposedViewSize,
        subviews: Subviews,
        cache: inout ()
    ) {
        guard subviews.count >= 2 else { return }

        // Mini mode is a true player window over the full terminal. Both
        // stateful UIKit children stay in this one Layout tree; only their
        // proposals and positions change, so the SSH PTY, scrollback, WebRTC
        // receiver, and one-use attach credential all survive the transition.
        if !playerExpanded {
            subviews[0].place(
                at: bounds.origin,
                anchor: .topLeading,
                proposal: ProposedViewSize(width: bounds.width, height: bounds.height)
            )

            let inset: CGFloat = 16
            let maximumWidth = max(bounds.width - inset * 2, 0)
            let previewWidth = min(max(bounds.width * 0.42, 300), min(520, maximumWidth))
            let previewHeight = min(
                max(previewWidth * 0.68, 220),
                max(bounds.height * 0.52, 0)
            )
            subviews[1].place(
                at: CGPoint(x: bounds.maxX - inset, y: bounds.maxY - inset),
                anchor: .bottomTrailing,
                proposal: ProposedViewSize(width: previewWidth, height: previewHeight)
            )

            for subview in subviews.dropFirst(2) {
                subview.place(
                    at: bounds.origin,
                    anchor: .topLeading,
                    proposal: ProposedViewSize(width: 0, height: 0)
                )
            }
            return
        }

        let horizontal = bounds.width > bounds.height
        if horizontal {
            let availableWidth = max(0, bounds.width - spacing)
            let terminalRatio: CGFloat = playerExpanded ? 0.36 : 0.68
            let terminalWidth = availableWidth * terminalRatio
            let previewWidth = availableWidth - terminalWidth

            subviews[0].place(
                at: bounds.origin,
                anchor: .topLeading,
                proposal: ProposedViewSize(
                    width: terminalWidth,
                    height: bounds.height
                )
            )
            subviews[1].place(
                at: CGPoint(x: bounds.minX + terminalWidth + spacing, y: bounds.minY),
                anchor: .topLeading,
                proposal: ProposedViewSize(
                    width: previewWidth,
                    height: bounds.height
                )
            )
        } else {
            let availableHeight = max(0, bounds.height - spacing)
            let terminalRatio: CGFloat = playerExpanded ? 0.40 : 0.64
            let terminalHeight = availableHeight * terminalRatio
            let previewHeight = availableHeight - terminalHeight

            subviews[0].place(
                at: bounds.origin,
                anchor: .topLeading,
                proposal: ProposedViewSize(
                    width: bounds.width,
                    height: terminalHeight
                )
            )
            subviews[1].place(
                at: CGPoint(x: bounds.minX, y: bounds.minY + terminalHeight + spacing),
                anchor: .topLeading,
                proposal: ProposedViewSize(
                    width: bounds.width,
                    height: previewHeight
                )
            )
        }

        for subview in subviews.dropFirst(2) {
            subview.place(
                at: bounds.origin,
                anchor: .topLeading,
                proposal: ProposedViewSize(width: 0, height: 0)
            )
        }
    }
}
