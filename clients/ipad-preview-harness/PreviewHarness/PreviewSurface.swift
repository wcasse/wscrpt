import SwiftUI

enum PreviewState: Equatable, Sendable {
    case idle
    case connecting
    case playing
    case failed(String)
}

struct PreviewMetrics: Equatable, Sendable {
    let presentedFPS: Double?
    let width: Int?
    let height: Int?
    let latencyMilliseconds: Double?
    let profile: String?
}

@MainActor
protocol PreviewSurfaceController: AnyObject {
    var state: PreviewState { get }
    var stateDidChange: ((PreviewState) -> Void)? { get set }
    var metricsDidChange: ((PreviewMetrics) -> Void)? { get set }

    func attach(
        _ session: PreviewSessionDescriptor,
        presentation: PreviewPresentation
    ) async throws
    func setPresentation(_ presentation: PreviewPresentation)
    func detach()
}

@MainActor
final class PreviewSurfaceModel: ObservableObject {
    @Published private(set) var state: PreviewState = .idle
    @Published private(set) var metrics: PreviewMetrics?
    @Published private(set) var presentation: PreviewPresentation = .mini
    @Published private(set) var configuration: PreviewLaunchConfiguration?

    private let controller: any PreviewSurfaceController
    private var attachmentEpoch: UInt64 = 0

    init(controller: any PreviewSurfaceController) {
        self.controller = controller
        controller.stateDidChange = { [weak self] state in
            self?.state = state
        }
        controller.metricsDidChange = { [weak self] metrics in
            self?.metrics = metrics
        }
    }

    func open(deepLink: URL) async {
        do {
            try await open(PreviewLaunchConfiguration.parse(deepLink: deepLink))
        } catch {
            close()
            state = .failed(error.localizedDescription)
        }
    }

    func open(_ newConfiguration: PreviewLaunchConfiguration) async throws {
        attachmentEpoch &+= 1
        let epoch = attachmentEpoch

        controller.detach()
        configuration = newConfiguration
        presentation = newConfiguration.presentation
        metrics = nil
        state = .connecting

        do {
            try await controller.attach(
                newConfiguration.session,
                presentation: newConfiguration.presentation
            )
            guard epoch == attachmentEpoch else {
                return
            }
            state = controller.state
        } catch {
            guard epoch == attachmentEpoch else {
                return
            }
            controller.detach()
            configuration = nil
            state = .failed(error.localizedDescription)
            throw error
        }
    }

    func setPresentation(_ newPresentation: PreviewPresentation) {
        presentation = newPresentation
        controller.setPresentation(newPresentation)
    }

    func close() {
        attachmentEpoch &+= 1
        controller.detach()
        configuration = nil
        metrics = nil
        state = .idle
    }
}

struct PreviewSurface: View {
    @ObservedObject var model: PreviewSurfaceModel
    let webSurface: WKWebRTCPreviewSurface

    var body: some View {
        VStack(spacing: 10) {
            ZStack {
                Color.black
                WKWebRTCPreviewView(surface: webSurface)
                    .allowsHitTesting(false)

                stateOverlay
            }
            .aspectRatio(16 / 9, contentMode: .fit)
            .frame(maxWidth: model.presentation == .mini ? 640 : .infinity)
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .stroke(Color.secondary.opacity(0.25), lineWidth: 1)
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Remote agent preview, view only")

            statusLine
        }
        .padding(model.presentation == .mini ? 16 : 0)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(uiColor: .systemBackground))
    }

    @ViewBuilder
    private var stateOverlay: some View {
        switch model.state {
        case .idle:
            Text("Choose a gameplay preview to attach")
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(14)
        case .connecting:
            ProgressView("Connecting to preview")
                .tint(.white)
                .foregroundStyle(.white)
                .padding(14)
                .background(.black.opacity(0.55), in: Capsule())
        case .playing:
            EmptyView()
        case let .failed(message):
            Text(message)
                .font(.callout)
                .foregroundStyle(.white)
                .multilineTextAlignment(.center)
                .padding(14)
                .background(.red.opacity(0.72), in: RoundedRectangle(cornerRadius: 8))
                .padding()
        }
    }

    @ViewBuilder
    private var statusLine: some View {
        if case .playing = model.state {
            HStack(spacing: 8) {
                Circle()
                    .fill(.green)
                    .frame(width: 7, height: 7)
                Text(statusText)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            .accessibilityLabel("Preview connected. \(statusText)")
        }
    }

    private var statusText: String {
        guard let metrics = model.metrics else {
            return model.presentation.rawValue
        }

        var values = [model.presentation.rawValue]
        if let width = metrics.width, let height = metrics.height {
            values.append("\(width)x\(height)")
        }
        if let fps = metrics.presentedFPS {
            values.append(String(format: "%.1f fps", fps))
        }
        if let latency = metrics.latencyMilliseconds {
            values.append(String(format: "%.0f ms", latency))
        }
        return values.joined(separator: "  ")
    }
}
