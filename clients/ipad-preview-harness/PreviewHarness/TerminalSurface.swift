import SwiftTerm
import SwiftUI
import UIKit

/// UI-facing SSH/PTTY state. The transport owns sockets and credentials; the
/// terminal owns only presentation, input forwarding, and resize reporting.
enum TerminalConnectionState: Equatable, Sendable {
    case disconnected
    case connecting
    case authenticating
    case openingPTY
    case connected
    case reconnecting(attempt: Int)
    case failed(String)

    var acceptsInput: Bool {
        if case .connected = self { return true }
        return false
    }

    var isBusy: Bool {
        switch self {
        case .connecting, .authenticating, .openingPTY, .reconnecting:
            return true
        case .disconnected, .connected, .failed:
            return false
        }
    }
}

struct TerminalDimensions: Equatable, Sendable {
    let columns: Int
    let rows: Int
    let pixelWidth: Int
    let pixelHeight: Int
}

/// The narrow seam the workspace session uses to bridge `SSHTransport`.
/// Keeping it at the terminal boundary prevents SwiftUI from knowing about
/// NIO channels, authentication, host-key prompts, or reconnect policy.
@MainActor
protocol TerminalSurfaceDelegate: AnyObject {
    func terminalSurface(
        _ surface: TerminalSurfaceController,
        didSend data: ArraySlice<UInt8>
    )

    func terminalSurface(
        _ surface: TerminalSurfaceController,
        didResize dimensions: TerminalDimensions
    )
}

/// Owns exactly one UIKit terminal for the lifetime of a workspace scene.
/// SwiftUI may recompute its value tree, but it never owns or recreates this
/// `TerminalView` instance.
@MainActor
final class TerminalSurfaceController: NSObject, ObservableObject {
    @Published private(set) var connectionState: TerminalConnectionState = .disconnected
    @Published private(set) var terminalTitle = "Terminal"
    @Published private(set) var currentDirectory: String?
    @Published private(set) var dimensions = TerminalDimensions(
        columns: 80,
        rows: 24,
        pixelWidth: 0,
        pixelHeight: 0
    )

    weak var delegate: (any TerminalSurfaceDelegate)?

    private(set) lazy var terminalView: TerminalView = {
        let view = TerminalView(
            frame: .zero,
            font: UIFont.monospacedSystemFont(ofSize: 13, weight: .regular)
        )
        view.terminalDelegate = self
        view.nativeBackgroundColor = .systemBackground
        view.nativeForegroundColor = .label
        view.caretColor = .systemBlue
        view.indicatorStyle = .default
        view.optionAsMetaKey = true
        view.allowMouseReporting = true
        view.changeScrollback(10_000)
        view.accessibilityLabel = "Remote wscrpt terminal"
        view.accessibilityHint = "Double tap to focus the terminal."
        return view
    }()

    func updateConnectionState(_ state: TerminalConnectionState) {
        connectionState = state
    }

    /// Delivers SSH PTY output into the emulator without changing view identity.
    func receive(_ bytes: ArraySlice<UInt8>) {
        terminalView.feed(byteArray: bytes)
    }

    func receive(_ data: Data) {
        receive(ArraySlice(data))
    }

    /// Explicit focus is exposed for the status-bar action and Magic Keyboard
    /// shortcut. Preview resize never resigns this responder.
    @discardableResult
    func focus() -> Bool {
        terminalView.becomeFirstResponder()
    }

    func clearDisplay() {
        terminalView.feed(text: "\u{001B}c")
    }

    private func makeDimensions(columns: Int, rows: Int) -> TerminalDimensions {
        let scale = terminalView.window?.screen.scale ?? UIScreen.main.scale
        return TerminalDimensions(
            columns: max(columns, 1),
            rows: max(rows, 1),
            pixelWidth: max(Int((terminalView.bounds.width * scale).rounded()), 0),
            pixelHeight: max(Int((terminalView.bounds.height * scale).rounded()), 0)
        )
    }
}

extension TerminalSurfaceController: @preconcurrency TerminalViewDelegate {
    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        let newDimensions = makeDimensions(columns: newCols, rows: newRows)
        guard newDimensions != dimensions else { return }
        dimensions = newDimensions
        delegate?.terminalSurface(self, didResize: newDimensions)
    }

    func setTerminalTitle(source: TerminalView, title: String) {
        let sanitized = title.unicodeScalars
            .filter({ !CharacterSet.controlCharacters.contains($0) })
            .prefix(256)
        let value = String(String.UnicodeScalarView(sanitized))
            .trimmingCharacters(in: .whitespacesAndNewlines)
        terminalTitle = value.isEmpty ? "Terminal" : value
    }

    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {
        guard let directory else {
            currentDirectory = nil
            return
        }
        let sanitized = directory.unicodeScalars
            .filter({ !CharacterSet.controlCharacters.contains($0) })
            .prefix(4_096)
        currentDirectory = String(String.UnicodeScalarView(sanitized))
    }

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        guard connectionState.acceptsInput else { return }
        delegate?.terminalSurface(self, didSend: data)
    }

    func scrolled(source: TerminalView, position: Double) {}

    /// Remote terminal output never opens a URL without a separate native user
    /// confirmation flow.
    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {}

    func bell(source: TerminalView) {}

    /// OSC 52 clipboard access fails closed. A later UI can add an explicit
    /// confirmation affordance without weakening this default.
    func clipboardCopy(source: TerminalView, content: Data) {}

    func clipboardRead(source: TerminalView) -> Data? { nil }

    func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}

    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
}

/// A stable bridge around the controller-owned terminal view.
struct TerminalSurface: UIViewRepresentable {
    @ObservedObject var controller: TerminalSurfaceController
    let focusRequest: UInt64
    let isInteractive: Bool

    init(
        controller: TerminalSurfaceController,
        focusRequest: UInt64 = 0,
        isInteractive: Bool = true
    ) {
        self.controller = controller
        self.focusRequest = focusRequest
        self.isInteractive = isInteractive
    }

    final class Coordinator {
        var handledFocusRequest: UInt64 = 0
    }

    func makeCoordinator() -> Coordinator {
        Coordinator()
    }

    func makeUIView(context: Context) -> TerminalView {
        let view = controller.terminalView
        view.isUserInteractionEnabled = isInteractive
        context.coordinator.handledFocusRequest = focusRequest
        if focusRequest > 0, isInteractive {
            DispatchQueue.main.async { [weak controller] in
                _ = controller?.focus()
            }
        }
        return view
    }

    func updateUIView(_ terminalView: TerminalView, context: Context) {
        // SwiftTerm performs its grid resize from layoutSubviews. There is no
        // state-driven replacement or reset here, so scrollback and responder
        // state survive every mini/expanded preview transition.
        terminalView.isUserInteractionEnabled = isInteractive
        guard isInteractive,
              context.coordinator.handledFocusRequest != focusRequest
        else {
            return
        }
        context.coordinator.handledFocusRequest = focusRequest
        DispatchQueue.main.async { [weak controller] in
            _ = controller?.focus()
        }
    }

    static func dismantleUIView(_ terminalView: TerminalView, coordinator: Coordinator) {
        // The workspace/transport lifecycle owns disconnect. SwiftUI teardown
        // must not implicitly close a live SSH session during scene changes.
    }
}
