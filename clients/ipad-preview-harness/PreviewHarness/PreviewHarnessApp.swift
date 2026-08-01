import SwiftUI

@main
struct PreviewHarnessApp: App {
    var body: some Scene {
        WindowGroup {
            PreviewHarnessRootView()
        }
    }
}

@MainActor
private struct PreviewHarnessRootView: View {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var model = WorkspaceSessionModel()
    @StateObject private var physicalKeyboard = PhysicalKeyboardMonitor()
    @StateObject private var privacyWindow = WorkspacePrivacyWindowController()

    var body: some View {
        ZStack {
            CombinedWorkspaceView(
                terminalController: model.terminalController,
                previewModel: model.previewModel,
                webSurface: model.webSurface,
                connectionState: model.connectionState,
                activeProfile: model.activeProfile,
                previewSessions: model.previewSessions,
                previewSessionState: model.previewSessionState,
                devicePublicKey: model.devicePublicKey,
                physicalKeyboardAvailability: physicalKeyboard.availability,
                onConnect: model.connect,
                onDisconnect: model.disconnect,
                onRefreshPreviewSessions: model.refreshPreviewSessions,
                onAttachPreviewSession: model.attachPreviewSession,
                onDetachPreviewSession: model.detachPreviewSession,
                onPrepareDeviceKey: model.prepareDeviceKey
            )

            if scenePhase != .active {
                WorkspacePrivacyShield()
                    .transition(.opacity)
                    .zIndex(100)
            }
        }
            .background {
                WorkspacePrivacyWindowAttachment(
                    isVisible: scenePhase != .active,
                    controller: privacyWindow
                )
                .frame(width: 0, height: 0)
            }
            .onChange(of: scenePhase) { _, phase in
                model.handleScenePhase(phase)
            }
            .alert(item: $model.hostKeyPrompt) { prompt in
                Alert(
                    title: Text("Verify SSH host key"),
                    message: Text(hostKeyMessage(prompt)),
                    primaryButton: .default(Text("Trust and connect")) {
                        model.resolveHostKeyPrompt(accepted: true)
                    },
                    secondaryButton: .cancel {
                        model.resolveHostKeyPrompt(accepted: false)
                    }
                )
            }
            .alert(item: $model.notice) { notice in
                Alert(
                    title: Text(notice.title),
                    message: Text(notice.message),
                    dismissButton: .default(Text("OK"))
                )
            }
    }

    private func hostKeyMessage(_ prompt: SSHHostKeyPrompt) -> String {
        let host = prompt.endpoint.host.contains(":")
            ? "[\(prompt.endpoint.host)]"
            : prompt.endpoint.host
        return "\(host):\(prompt.endpoint.port) presented \(prompt.presentedKey.fingerprint.algorithm) \(prompt.presentedKey.fingerprint.description). Confirm this fingerprint through a trusted route before accepting."
    }
}

/// Owns a non-key window above SwiftUI sheets and alerts. The in-hierarchy
/// shield remains as an immediate fallback, while this window ensures scene
/// snapshots cannot expose metadata presented outside the root view's ZStack.
@MainActor
private final class WorkspacePrivacyWindowController: ObservableObject {
    private weak var windowScene: UIWindowScene?
    private var shieldWindow: UIWindow?

    func setVisible(_ isVisible: Bool, in scene: UIWindowScene) {
        if let windowScene, windowScene !== scene {
            shieldWindow?.isHidden = true
            shieldWindow = nil
        }
        windowScene = scene

        guard isVisible else {
            shieldWindow?.isHidden = true
            return
        }

        if shieldWindow == nil {
            let window = UIWindow(windowScene: scene)
            window.frame = scene.coordinateSpace.bounds
            window.windowLevel = UIWindow.Level(
                rawValue: UIWindow.Level.alert.rawValue + 1
            )
            window.backgroundColor = .systemBackground
            window.rootViewController = UIHostingController(
                rootView: WorkspacePrivacyShield()
            )
            window.accessibilityViewIsModal = true
            shieldWindow = window
        }
        shieldWindow?.frame = scene.coordinateSpace.bounds
        shieldWindow?.isHidden = false
    }

    func hide() {
        shieldWindow?.isHidden = true
    }
}

private struct WorkspacePrivacyWindowAttachment: UIViewRepresentable {
    let isVisible: Bool
    let controller: WorkspacePrivacyWindowController

    func makeUIView(context: Context) -> WorkspacePrivacyAttachmentView {
        let view = WorkspacePrivacyAttachmentView()
        view.controller = controller
        view.isShieldVisible = isVisible
        return view
    }

    func updateUIView(_ view: WorkspacePrivacyAttachmentView, context: Context) {
        view.controller = controller
        view.isShieldVisible = isVisible
        view.synchronize()
    }

    static func dismantleUIView(
        _ view: WorkspacePrivacyAttachmentView,
        coordinator: Void
    ) {
        view.controller?.hide()
    }
}

private final class WorkspacePrivacyAttachmentView: UIView {
    weak var controller: WorkspacePrivacyWindowController?
    var isShieldVisible = false

    override func didMoveToWindow() {
        super.didMoveToWindow()
        synchronize()
    }

    func synchronize() {
        guard let scene = window?.windowScene else { return }
        controller?.setVisible(isShieldVisible, in: scene)
    }
}

private struct WorkspacePrivacyShield: View {
    var body: some View {
        ZStack {
            Color(uiColor: .systemBackground)
                .ignoresSafeArea()
            Label("Workspace hidden", systemImage: "lock.fill")
                .font(.headline)
                .foregroundStyle(.secondary)
                .accessibilityLabel("Remote workspace hidden while wscrpt is inactive")
        }
    }
}
