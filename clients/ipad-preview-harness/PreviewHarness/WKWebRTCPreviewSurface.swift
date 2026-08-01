import SwiftUI
@preconcurrency import WebKit

enum WKPreviewSurfaceError: Error, LocalizedError {
    case attachmentReplaced
    case navigationFailed
    case browserContractUnavailable

    var errorDescription: String? {
        switch self {
        case .attachmentReplaced:
            return "The preview attachment was replaced."
        case .navigationFailed:
            return "The loopback preview page could not be loaded."
        case .browserContractUnavailable:
            return "The loopback page does not provide a compatible preview surface."
        }
    }
}

enum WKPreviewBridgeEvent: Equatable {
    case state(String)
    case metrics(PreviewMetrics)
}

/// Strict, shallow decoding avoids recursively re-serializing arbitrary page
/// objects on the MainActor. WebKit has already materialized `message.body` by
/// delegate delivery, so this schema and the rate limiter below are the native
/// allocation/dispatch boundary rather than the encoded-size check alone.
enum WKPreviewBridgePayload {
    static func parse(_ value: Any) -> WKPreviewBridgeEvent? {
        guard let body = value as? [String: Any],
              let type = body["type"] as? String
        else {
            return nil
        }

        switch type {
        case "state":
            let keys = Set(body.keys)
            guard keys == ["type", "state"]
                    || keys == ["type", "state", "message"],
                  let state = body["state"] as? String,
                  ["connecting", "playing", "closed", "idle", "error"]
                    .contains(state),
                  state.utf8.count <= 16
            else {
                return nil
            }
            if let message = body["message"] {
                guard let message = message as? String,
                      message.utf8.count <= 512
                else {
                    return nil
                }
            }
            return .state(state)

        case "metrics":
            guard Set(body.keys) == ["type", "metrics"],
                  let raw = body["metrics"] as? [String: Any]
            else {
                return nil
            }
            let required: Set<String> = [
                "presentedFps", "width", "height", "profile",
            ]
            let allowed = required.union(["latencyMs"])
            guard required.isSubset(of: raw.keys),
                  Set(raw.keys).isSubset(of: allowed),
                  let presentedFPS = number(
                      raw["presentedFps"],
                      range: 0 ... 1_000
                  ),
                  let width = integer(raw["width"], range: 0 ... 16_384),
                  let height = integer(raw["height"], range: 0 ... 16_384),
                  let profile = raw["profile"] as? String,
                  (1 ... 64).contains(profile.utf8.count),
                  !profile.unicodeScalars.contains(where: {
                      CharacterSet.controlCharacters.contains($0)
                  })
            else {
                return nil
            }
            let latency: Double?
            if let rawLatency = raw["latencyMs"] {
                guard let parsed = number(
                    rawLatency,
                    range: 0 ... 600_000
                ) else {
                    return nil
                }
                latency = parsed
            } else {
                latency = nil
            }
            return .metrics(
                PreviewMetrics(
                    presentedFPS: presentedFPS,
                    width: width,
                    height: height,
                    latencyMilliseconds: latency,
                    profile: profile
                )
            )

        default:
            return nil
        }
    }

    private static func number(
        _ value: Any?,
        range: ClosedRange<Double>
    ) -> Double? {
        guard let number = value as? NSNumber,
              CFGetTypeID(number) != CFBooleanGetTypeID()
        else {
            return nil
        }
        let value = number.doubleValue
        guard value.isFinite, range.contains(value) else { return nil }
        return value
    }

    private static func integer(
        _ value: Any?,
        range: ClosedRange<Int>
    ) -> Int? {
        guard let number = number(
            value,
            range: Double(range.lowerBound) ... Double(range.upperBound)
        ), number.rounded(.towardZero) == number
        else {
            return nil
        }
        return Int(number)
    }
}

struct WKPreviewBridgeRateLimiter {
    static let maximumMessagesPerWindow = 64
    static let windowSeconds: TimeInterval = 1

    private var windowStartedAt: TimeInterval?
    private var messageCount = 0

    mutating func admit(at now: TimeInterval) -> Bool {
        guard now.isFinite else { return false }
        if let windowStartedAt,
           now >= windowStartedAt,
           now - windowStartedAt < Self.windowSeconds
        {
            messageCount += 1
        } else {
            windowStartedAt = now
            messageCount = 1
        }
        return messageCount <= Self.maximumMessagesPerWindow
    }

    mutating func reset() {
        windowStartedAt = nil
        messageCount = 0
    }
}

struct WKPreviewNavigationIdentity: Equatable {
    private let navigationIdentifier: ObjectIdentifier
    let attachmentEpoch: UInt64
    let requestID: UUID

    init(
        navigation: AnyObject,
        attachmentEpoch: UInt64,
        requestID: UUID
    ) {
        navigationIdentifier = ObjectIdentifier(navigation)
        self.attachmentEpoch = attachmentEpoch
        self.requestID = requestID
    }

    func matches(
        navigation: AnyObject?,
        attachmentEpoch: UInt64
    ) -> Bool {
        guard let navigation else {
            return false
        }
        return navigationIdentifier == ObjectIdentifier(navigation)
            && self.attachmentEpoch == attachmentEpoch
    }
}

struct WKPreviewPendingNavigationTracker<Value> {
    private struct PendingNavigation {
        let identity: WKPreviewNavigationIdentity
        let value: Value
    }

    private var pending: PendingNavigation?

    var hasPending: Bool {
        pending != nil
    }

    @discardableResult
    mutating func install(
        navigation: AnyObject,
        attachmentEpoch: UInt64,
        requestID: UUID,
        value: Value
    ) -> Value? {
        let replaced = pending?.value
        pending = PendingNavigation(
            identity: WKPreviewNavigationIdentity(
                navigation: navigation,
                attachmentEpoch: attachmentEpoch,
                requestID: requestID
            ),
            value: value
        )
        return replaced
    }

    mutating func take(
        matching navigation: AnyObject?,
        attachmentEpoch: UInt64
    ) -> Value? {
        guard let pending,
              pending.identity.matches(
                  navigation: navigation,
                  attachmentEpoch: attachmentEpoch
              )
        else {
            return nil
        }
        self.pending = nil
        return pending.value
    }

    mutating func take(requestID: UUID) -> Value? {
        guard let pending,
              pending.identity.requestID == requestID
        else {
            return nil
        }
        self.pending = nil
        return pending.value
    }

    mutating func takeCurrent(attachmentEpoch: UInt64) -> Value? {
        guard let pending,
              pending.identity.attachmentEpoch == attachmentEpoch
        else {
            return nil
        }
        self.pending = nil
        return pending.value
    }

    mutating func takeAny() -> Value? {
        guard let pending else {
            return nil
        }
        self.pending = nil
        return pending.value
    }
}

@MainActor
final class WKWebRTCPreviewSurface: NSObject, PreviewSurfaceController {
    static let messageHandlerName = "preview"
    static let minimumMetricsInterval: TimeInterval = 0.9
    private static let blankPageHTML = """
        <!doctype html>
        <meta name="color-scheme" content="dark">
        <style>
          html, body { width: 100%; height: 100%; margin: 0; background: #000; }
        </style>
        <body></body>
        """

    private(set) var state: PreviewState = .idle
    var stateDidChange: ((PreviewState) -> Void)?
    var metricsDidChange: ((PreviewMetrics) -> Void)?

    private var allowedPageURL: URL?
    private var allowedReceiverURL: URL?
    private var pendingNavigation = WKPreviewPendingNavigationTracker<
        CheckedContinuation<Void, Error>
    >()
    private var attachmentEpoch: UInt64 = 0
    private var lastMetricsReceipt: TimeInterval?
    private var bridgeRateLimiter = WKPreviewBridgeRateLimiter()

    private(set) lazy var webView: WKWebView = makeWebView()

    func attach(
        _ session: PreviewSessionDescriptor,
        presentation: PreviewPresentation
    ) async throws {
        attachmentEpoch &+= 1
        let epoch = attachmentEpoch
        detachCurrent(clearPage: false)

        let receiverURL = try session.receiverURL(presentation: presentation)
        guard PreviewSessionDescriptor.isStrictLoopbackPageURL(session.playerURL),
              Self.isAllowedFrameURL(
                  receiverURL,
                  cleanPageURL: session.playerURL,
                  receiverURL: receiverURL
              )
        else {
            throw PreviewConfigurationError.nonLoopbackURL
        }

        allowedPageURL = session.playerURL
        allowedReceiverURL = receiverURL
        lastMetricsReceipt = nil
        bridgeRateLimiter.reset()
        transition(to: .connecting)

        var request = URLRequest(
            url: receiverURL,
            cachePolicy: .reloadIgnoringLocalAndRemoteCacheData,
            timeoutInterval: 15
        )
        request.httpShouldHandleCookies = false

        do {
            try await load(request, attachmentEpoch: epoch)
            guard epoch == attachmentEpoch else {
                throw WKPreviewSurfaceError.attachmentReplaced
            }

            let contract = try await webView.evaluateJavaScript(
                "Boolean(window.wscrptPreview && window.wscrptPreview.attach && window.wscrptPreview.detach && window.wscrptPreview.setPresentation)"
            )
            guard contract as? Bool == true else {
                throw WKPreviewSurfaceError.browserContractUnavailable
            }
        } catch {
            guard epoch == attachmentEpoch else {
                throw WKPreviewSurfaceError.attachmentReplaced
            }
            transition(to: .failed(error.localizedDescription))
            throw error
        }
    }

    func setPresentation(_ presentation: PreviewPresentation) {
        guard allowedPageURL != nil else {
            return
        }
        let epoch = attachmentEpoch
        Task { @MainActor [weak self] in
            guard let self else {
                return
            }
            do {
                _ = try await webView.callAsyncJavaScript(
                    "return await window.wscrptPreview.setPresentation(presentation);",
                    arguments: ["presentation": presentation.rawValue],
                    in: nil,
                    contentWorld: .page
                )
            } catch {
                guard epoch == attachmentEpoch else {
                    return
                }
                transition(to: .failed("The preview presentation could not be changed."))
            }
        }
    }

    func detach() {
        attachmentEpoch &+= 1
        detachCurrent(clearPage: true)
    }

    private func detachCurrent(clearPage: Bool) {
        if let continuation = pendingNavigation.takeAny() {
            continuation.resume(throwing: WKPreviewSurfaceError.attachmentReplaced)
        }

        webView.stopLoading()
        let epoch = attachmentEpoch
        webView.evaluateJavaScript("window.wscrptPreview?.detach?.();") { [weak self] _, _ in
            guard clearPage,
                  let self,
                  self.attachmentEpoch == epoch,
                  self.allowedPageURL == nil
            else {
                return
            }
            self.webView.loadHTMLString(
                Self.blankPageHTML,
                baseURL: nil
            )
        }

        allowedPageURL = nil
        allowedReceiverURL = nil
        lastMetricsReceipt = nil
        bridgeRateLimiter.reset()
        transition(to: .idle)
    }

    private func load(
        _ request: URLRequest,
        attachmentEpoch epoch: UInt64
    ) async throws {
        let requestID = UUID()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation {
                (continuation: CheckedContinuation<Void, Error>) in
                guard epoch == attachmentEpoch else {
                    continuation.resume(
                        throwing: WKPreviewSurfaceError.attachmentReplaced
                    )
                    return
                }
                guard !Task.isCancelled else {
                    continuation.resume(throwing: CancellationError())
                    return
                }
                guard let navigation = webView.load(request) else {
                    continuation.resume(
                        throwing: WKPreviewSurfaceError.navigationFailed
                    )
                    return
                }

                if let replaced = pendingNavigation.install(
                    navigation: navigation,
                    attachmentEpoch: epoch,
                    requestID: requestID,
                    value: continuation
                ) {
                    replaced.resume(
                        throwing: WKPreviewSurfaceError.attachmentReplaced
                    )
                }

                if Task.isCancelled {
                    cancelPendingNavigation(requestID: requestID)
                }
            }
        } onCancel: {
            Task { @MainActor [weak self] in
                self?.cancelPendingNavigation(requestID: requestID)
            }
        }
        try Task.checkCancellation()
    }

    private func cancelPendingNavigation(requestID: UUID) {
        guard let continuation = pendingNavigation.take(requestID: requestID) else {
            return
        }
        webView.stopLoading()
        continuation.resume(throwing: CancellationError())
    }

    private func makeWebView() -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.allowsInlineMediaPlayback = true
        configuration.mediaTypesRequiringUserActionForPlayback = []
        configuration.allowsAirPlayForMediaPlayback = false
        configuration.allowsPictureInPictureMediaPlayback = false
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false

        let userContentController = WKUserContentController()
        userContentController.add(
            WeakScriptMessageHandler(delegate: self),
            name: Self.messageHandlerName
        )
        userContentController.addUserScript(
            WKUserScript(
                source: """
                document.addEventListener('DOMContentLoaded', () => {
                  document.documentElement.style.webkitUserSelect = 'none';
                  document.documentElement.style.webkitTouchCallout = 'none';
                  for (const video of document.querySelectorAll('video')) video.controls = false;
                }, { once: true });
                """,
                injectionTime: .atDocumentStart,
                forMainFrameOnly: true
            )
        )
        configuration.userContentController = userContentController

        let view = WKWebView(frame: .zero, configuration: configuration)
        view.navigationDelegate = self
        view.uiDelegate = self
        view.isOpaque = true
        view.underPageBackgroundColor = .black
        view.backgroundColor = .black
        view.scrollView.backgroundColor = .black
        view.scrollView.isScrollEnabled = false
        view.scrollView.bounces = false
        view.scrollView.contentInsetAdjustmentBehavior = .never
        view.allowsLinkPreview = false
        view.isUserInteractionEnabled = false
        view.loadHTMLString(Self.blankPageHTML, baseURL: nil)
        return view
    }

    private func transition(to newState: PreviewState) {
        guard state != newState else { return }
        state = newState
        stateDidChange?(newState)
    }

    static func isAllowedNavigation(_ candidate: URL, for allowedPageURL: URL) -> Bool {
        guard let candidateComponents = URLComponents(
                  url: candidate,
                  resolvingAgainstBaseURL: false
              ),
              let allowedComponents = URLComponents(
                  url: allowedPageURL,
                  resolvingAgainstBaseURL: false
              ),
              candidateComponents.scheme?.lowercased() == "http",
              candidateComponents.host == "127.0.0.1",
              candidateComponents.scheme?.lowercased() == allowedComponents.scheme?.lowercased(),
              candidateComponents.host == allowedComponents.host,
              candidateComponents.port == allowedComponents.port,
              candidateComponents.user == nil,
              candidateComponents.password == nil,
              candidateComponents.query == nil,
              candidateComponents.fragment == nil,
              candidateComponents.path == allowedComponents.path
        else {
            return false
        }
        return true
    }

    private static func isAllowedFrameURL(
        _ candidate: URL,
        cleanPageURL: URL,
        receiverURL: URL
    ) -> Bool {
        if isAllowedNavigation(candidate, for: cleanPageURL) {
            return true
        }
        return candidate.absoluteString == receiverURL.absoluteString
    }

    private func receiveBridgeMessage(_ message: WKScriptMessage) {
        guard message.name == Self.messageHandlerName,
              message.frameInfo.isMainFrame,
              let allowedPageURL,
              let allowedReceiverURL,
              let frameURL = message.frameInfo.request.url,
              Self.isAllowedFrameURL(
                  frameURL,
                  cleanPageURL: allowedPageURL,
                  receiverURL: allowedReceiverURL
              )
        else {
            return
        }

        let now = Date.timeIntervalSinceReferenceDate
        guard bridgeRateLimiter.admit(at: now) else {
            attachmentEpoch &+= 1
            detachCurrent(clearPage: true)
            transition(
                to: .failed("The remote preview exceeded the native message limit.")
            )
            return
        }
        guard let event = WKPreviewBridgePayload.parse(message.body) else {
            return
        }

        switch event {
        case let .state(value):
            receiveState(value)
        case let .metrics(metrics):
            receiveMetrics(metrics, at: now)
        }
    }

    private func receiveState(_ value: String?) {
        switch value {
        case "connecting":
            transition(to: .connecting)
        case "playing":
            transition(to: .playing)
        case "closed", "idle":
            transition(to: .idle)
        case "error":
            transition(to: .failed("The remote preview reported an error."))
        default:
            break
        }
    }

    private func receiveMetrics(
        _ metrics: PreviewMetrics,
        at now: TimeInterval
    ) {
        if let lastMetricsReceipt,
           now - lastMetricsReceipt < Self.minimumMetricsInterval
        {
            return
        }
        lastMetricsReceipt = now
        metricsDidChange?(metrics)
    }
}

extension WKWebRTCPreviewSurface: WKNavigationDelegate {
    func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping @MainActor @Sendable (
            WKNavigationActionPolicy
        ) -> Void
    ) {
        guard let url = navigationAction.request.url else {
            decisionHandler(.cancel)
            return
        }

        if url.absoluteString == "about:blank", allowedPageURL == nil {
            decisionHandler(.allow)
            return
        }

        guard pendingNavigation.hasPending,
              navigationAction.targetFrame?.isMainFrame == true,
              navigationAction.navigationType != .linkActivated,
              let allowedReceiverURL,
              url.absoluteString == allowedReceiverURL.absoluteString
        else {
            decisionHandler(.cancel)
            return
        }
        decisionHandler(.allow)
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        guard let continuation = pendingNavigation.take(
            matching: navigation,
            attachmentEpoch: attachmentEpoch
        ) else {
            return
        }
        continuation.resume()
    }

    func webView(
        _ webView: WKWebView,
        didFailProvisionalNavigation navigation: WKNavigation!,
        withError error: Error
    ) {
        failPendingNavigation(matching: navigation)
    }

    func webView(
        _ webView: WKWebView,
        didFail navigation: WKNavigation!,
        withError error: Error
    ) {
        failPendingNavigation(matching: navigation)
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        guard allowedPageURL != nil else {
            return
        }
        failCurrentPendingNavigation()
        transition(to: .failed("The preview web process stopped."))
    }

    private func failPendingNavigation(matching navigation: WKNavigation?) {
        guard let continuation = pendingNavigation.take(
            matching: navigation,
            attachmentEpoch: attachmentEpoch
        ) else {
            return
        }
        continuation.resume(throwing: WKPreviewSurfaceError.navigationFailed)
    }

    private func failCurrentPendingNavigation() {
        guard let continuation = pendingNavigation.takeCurrent(
            attachmentEpoch: attachmentEpoch
        ) else {
            return
        }
        continuation.resume(throwing: WKPreviewSurfaceError.navigationFailed)
    }
}

extension WKWebRTCPreviewSurface: WKUIDelegate {
    func webView(
        _ webView: WKWebView,
        requestMediaCapturePermissionFor origin: WKSecurityOrigin,
        initiatedByFrame frame: WKFrameInfo,
        type: WKMediaCaptureType,
        decisionHandler: @escaping @MainActor @Sendable (
            WKPermissionDecision
        ) -> Void
    ) {
        decisionHandler(.deny)
    }

    func webView(
        _ webView: WKWebView,
        requestDeviceOrientationAndMotionPermissionFor origin: WKSecurityOrigin,
        initiatedByFrame frame: WKFrameInfo,
        decisionHandler: @escaping @MainActor @Sendable (
            WKPermissionDecision
        ) -> Void
    ) {
        decisionHandler(.deny)
    }
}

extension WKWebRTCPreviewSurface: WKScriptMessageHandler {
    func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        receiveBridgeMessage(message)
    }
}

private final class WeakScriptMessageHandler: NSObject, WKScriptMessageHandler {
    weak var delegate: (any WKScriptMessageHandler)?

    init(delegate: any WKScriptMessageHandler) {
        self.delegate = delegate
    }

    func userContentController(
        _ userContentController: WKUserContentController,
        didReceive message: WKScriptMessage
    ) {
        delegate?.userContentController(userContentController, didReceive: message)
    }
}

struct WKWebRTCPreviewView: UIViewRepresentable {
    let surface: WKWebRTCPreviewSurface

    final class Coordinator {
        let surface: WKWebRTCPreviewSurface

        init(surface: WKWebRTCPreviewSurface) {
            self.surface = surface
        }
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(surface: surface)
    }

    func makeUIView(context: Context) -> WKWebView {
        surface.webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {}

    static func dismantleUIView(_ webView: WKWebView, coordinator: Coordinator) {
        // SwiftUI may dismantle and rebuild representable wrappers during an
        // adaptive layout transition. Attachment lifetime belongs to
        // PreviewSurfaceModel, not to this transient wrapper: detaching here
        // would burn the one-use receiver credential when expanding or
        // collapsing the player.
    }
}
