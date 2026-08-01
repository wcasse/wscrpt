import {
  FixtureLatencyProbe,
  LatestValue,
  MetricsPump,
  PresentedFrameMetrics,
} from "./metrics.mjs";
import {
  PREVIEW_PROFILES,
  WebRtcPreviewProvider,
  isLoopbackHostname,
} from "./webrtc-provider.mjs";
import { JpegPreviewProvider } from "./jpeg-provider.mjs";

const PRESENTATION_PROFILE = Object.freeze({ mini: "mini", expanded: "expanded" });

function combineAbortSignals(controller, signal) {
  if (!signal) return () => {};
  if (signal.aborted) {
    controller.abort(signal.reason);
    return () => {};
  }
  const abort = () => controller.abort(signal.reason);
  signal.addEventListener("abort", abort, { once: true });
  return () => signal.removeEventListener("abort", abort);
}

function safeMessage(error) {
  const message = error instanceof Error ? error.message : String(error);
  return message.slice(0, 512);
}

export function configurePreviewVideo(video) {
  video.autoplay = true;
  video.muted = true;
  video.defaultMuted = true;
  video.playsInline = true;
  video.controls = false;
  video.disablePictureInPicture = true;
  video.disableRemotePlayback = true;
  video.tabIndex = -1;
  video.setAttribute?.("autoplay", "");
  video.setAttribute?.("muted", "");
  video.setAttribute?.("playsinline", "");
  video.setAttribute?.("aria-label", "Remote agent preview");
  video.removeAttribute?.("controls");
  if (video.style) {
    video.style.pointerEvents = "none";
    video.style.userSelect = "none";
    video.style.webkitUserSelect = "none";
  }
  return video;
}

function createComposition(root, documentRef) {
  const frame = documentRef.createElement("div");
  frame.className = "preview-frame";
  frame.dataset.presentation = "mini";
  frame.setAttribute("aria-live", "polite");

  const video = configurePreviewVideo(documentRef.createElement("video"));
  video.className = "preview-video";
  const status = documentRef.createElement("div");
  status.className = "preview-status";
  status.textContent = "Preview idle";
  frame.append(video, status);
  root.append(frame);
  return { frame, video, status };
}

export class PreviewSurface {
  constructor(
    root,
    {
      providers = {},
      documentRef = globalThis.document,
      onMessage = () => {},
      metricsIntervalMs = 1_000,
      staleFrameAgeMs = 500,
      staleSampleLimit = 3,
    } = {},
  ) {
    if (!root || !documentRef) throw new Error("PreviewSurface requires a DOM root");
    this.root = root;
    this.documentRef = documentRef;
    this.providers = providers;
    this.onMessage = onMessage;
    this.metricsIntervalMs = metricsIntervalMs;
    this.staleFrameAgeMs = staleFrameAgeMs;
    this.staleSampleLimit = staleSampleLimit;
    this.lifecycle = 0;
    this.connection = null;
    this.provider = null;
    this.controller = null;
    this.detachExternalAbort = null;
    this.detachConnectionState = null;
    this.detachConnectionProfile = null;
    this.presented = null;
    this.metricsPump = null;
    this.latencyProbe = null;
    this.latencyChannel = null;
    this.detachLatencyFrame = null;
    this.lastLatencyMs = null;
    this.pendingLatencyMs = null;
    this.staleSamples = 0;
    this.restarting = false;
    this.profile = "mini";
    this.presentation = "mini";
    this.state = "closed";
    this.latestState = new LatestValue();
    this.latestMetrics = new LatestValue();
    const composition = createComposition(root, documentRef);
    this.frame = composition.frame;
    this.video = composition.video;
    this.status = composition.status;
  }

  resolveProvider(provider) {
    if (provider && typeof provider === "object" && typeof provider.connect === "function") {
      return provider;
    }
    const name = provider ?? "webrtc";
    const configured = this.providers[name];
    if (!configured) throw new Error(`preview provider is unavailable: ${name}`);
    return typeof configured === "function" ? configured() : configured;
  }

  emitState(state, message) {
    this.state = state;
    const payload = {
      type: "state",
      state,
      ...(message ? { message: String(message).slice(0, 512) } : {}),
    };
    this.latestState.publish(payload, this.lifecycle);
    this.frame.dataset.state = state;
    this.status.textContent =
      state === "playing"
        ? "Agent preview"
        : state === "connecting"
          ? "Connecting to agent preview…"
          : state === "error"
            ? `Preview unavailable${message ? `: ${message}` : ""}`
            : "Preview closed";
    this.onMessage(payload);
  }

  async open({
    session,
    provider = "webrtc",
    profile = "mini",
    presentation = profile === "mini" ? "mini" : "expanded",
    signal,
  } = {}) {
    if (!Object.hasOwn(PREVIEW_PROFILES, profile)) {
      throw new Error(`unknown preview profile: ${profile}`);
    }
    if (!Object.hasOwn(PRESENTATION_PROFILE, presentation)) {
      throw new Error(`unknown preview presentation: ${presentation}`);
    }
    this.close({ emit: false });
    const lifecycle = ++this.lifecycle;
    this.profile = profile;
    this.presentation = presentation;
    this.frame.dataset.presentation = presentation;
    this.frame.dataset.provider = typeof provider === "string" ? provider : "custom";
    this.controller = new AbortController();
    this.detachExternalAbort = combineAbortSignals(this.controller, signal);
    this.emitState("connecting");

    let connection;
    try {
      this.provider = this.resolveProvider(provider);
      connection = await this.provider.connect({
        session,
        profile,
        signal: this.controller.signal,
      });
      if (lifecycle !== this.lifecycle || this.controller.signal.aborted) {
        connection.close();
        throw this.controller.signal.reason ?? new Error("preview attach superseded");
      }
      this.connection = connection;
      this.detachConnectionState = connection.onState?.(({ state, message }) => {
        if (lifecycle !== this.lifecycle) return;
        if (state === "connected" || state === "playing") {
          this.restarting = false;
          this.staleSamples = 0;
          this.emitState("playing");
        } else if (state === "closed") {
          this.emitState("closed");
        } else if (state === "error" || state === "failed" || state === "disconnected") {
          this.emitState("error", message ?? `WebRTC ${state}`);
        } else {
          this.emitState("connecting");
        }
      });
      this.detachConnectionProfile = connection.onProfile?.((appliedProfile) => {
        if (lifecycle !== this.lifecycle || !Object.hasOwn(PREVIEW_PROFILES, appliedProfile)) {
          return;
        }
        this.profile = appliedProfile;
      });
      this.video.srcObject = connection.stream;
      this.presented = new PresentedFrameMetrics(this.video);
      this.presented.start();

      const initialGeneration = session.generation;
      this.metricsPump = new MetricsPump(
        {
          connection,
          presented: this.presented,
          profile: () => this.profile,
          generation: () => connection.getGeneration?.() ?? initialGeneration,
          onSample: (sample) => this.handleMetrics(sample, lifecycle),
        },
        { intervalMs: this.metricsIntervalMs },
      );
      this.metricsPump.start();
      try {
        await this.video.play?.();
      } catch (error) {
        throw new Error(`muted preview autoplay failed: ${safeMessage(error)}`);
      }
      return this;
    } catch (error) {
      if (lifecycle === this.lifecycle) {
        this.emitState("error", safeMessage(error));
        this.teardownResources();
      }
      throw error;
    }
  }

  handleMetrics(sample, lifecycle) {
    if (lifecycle !== this.lifecycle || !this.connection) return;
    const latencyChannel = this.connection.getLatencyChannel?.();
    if (this.latencyProbe && latencyChannel && latencyChannel !== this.latencyChannel) {
      this.latencyChannel = latencyChannel;
      this.latencyProbe.attach(latencyChannel);
    }
    const metrics = {
      ...sample,
      ...(this.lastLatencyMs === null ? {} : { latencyMs: this.lastLatencyMs }),
    };
    this.latestMetrics.publish(metrics, sample.generation ?? lifecycle);
    this.onMessage({ type: "metrics", metrics });
    // Startup samples commonly contain zero decoded/presented frames before
    // ontrack. They are useful to the local UI but must not trigger network
    // adaptation or enter the evidence ledger as playback measurements.
    if (this.state !== "playing") return;
    const packets = sample.packetsReceivedDelta;
    const lost = sample.packetLossDelta;
    const packetLossRatio =
      Number.isFinite(packets) && Number.isFinite(lost)
        ? lost / Math.max(1, packets + lost)
        : null;
    const pendingLatencyMs = this.pendingLatencyMs;
    try {
      this.connection.reportStats?.({
        presentedFps: Number.isFinite(sample.presentedFps) ? sample.presentedFps : null,
        decodedFps: Number.isFinite(sample.decodedFps) ? sample.decodedFps : null,
        frameAgeMs: Number.isFinite(sample.frameAgeMs)
          ? sample.frameAgeMs
          : Number.isFinite(sample.presentationAgeMs)
            ? sample.presentationAgeMs
            : null,
        presentationAgeMs: Number.isFinite(sample.presentationAgeMs)
          ? sample.presentationAgeMs
          : null,
        frameAgeBasis: sample.frameAgeBasis ?? "presentationGap",
        maxFreezeMs: Number.isFinite(sample.maxFreezeMs) ? sample.maxFreezeMs : null,
        packetLossRatio,
        packetLossDelta: Number.isFinite(sample.packetLossDelta)
          ? sample.packetLossDelta
          : null,
        packetsReceived: Number.isFinite(sample.packetsReceived)
          ? sample.packetsReceived
          : null,
        packetsReceivedDelta: Number.isFinite(sample.packetsReceivedDelta)
          ? sample.packetsReceivedDelta
          : null,
        packetsLost: Number.isFinite(sample.packetsLost) ? sample.packetsLost : null,
        framesDecoded: Number.isFinite(sample.framesDecoded) ? sample.framesDecoded : null,
        framesDropped: Number.isFinite(sample.framesDropped) ? sample.framesDropped : null,
        bytesReceived: Number.isFinite(sample.bytesReceived) ? sample.bytesReceived : null,
        bitrateBps: Number.isFinite(sample.bitrateBps) ? sample.bitrateBps : null,
        availableIncomingBitrate: Number.isFinite(sample.availableIncomingBitrate)
          ? sample.availableIncomingBitrate
          : null,
        jitterSeconds: Number.isFinite(sample.jitterSeconds) ? sample.jitterSeconds : null,
        codec: typeof sample.codec === "string" ? sample.codec : null,
        codecPayloadType: Number.isFinite(sample.codecPayloadType)
          ? sample.codecPayloadType
          : null,
        rttMs: Number.isFinite(sample.rttMs) ? sample.rttMs : null,
        ...(Number.isFinite(pendingLatencyMs) ? { latencyMs: pendingLatencyMs } : {}),
        width: sample.width ?? 0,
        height: sample.height ?? 0,
        localCandidateType:
          typeof sample.localCandidateType === "string" ? sample.localCandidateType : null,
        remoteCandidateType:
          typeof sample.remoteCandidateType === "string" ? sample.remoteCandidateType : null,
        profile: this.profile,
      });
      if (this.pendingLatencyMs === pendingLatencyMs) this.pendingLatencyMs = null;
    } catch (error) {
      if (lifecycle === this.lifecycle) this.emitState("error", safeMessage(error));
    }
  }

  async setPresentation(presentation) {
    if (!Object.hasOwn(PRESENTATION_PROFILE, presentation)) {
      throw new Error(`unknown preview presentation: ${presentation}`);
    }
    this.presentation = presentation;
    this.frame.dataset.presentation = presentation;
    return this.setProfile(PRESENTATION_PROFILE[presentation]);
  }

  async setProfile(profile) {
    if (!Object.hasOwn(PREVIEW_PROFILES, profile)) {
      throw new Error(`unknown preview profile: ${profile}`);
    }
    this.profile = profile;
    await this.connection?.setProfile(profile);
  }

  requestLatencyProbe() {
    if (!this.presented || !this.connection) throw new Error("preview is not attached");
    if (!this.latencyProbe) {
      this.latencyProbe = new FixtureLatencyProbe(this.video, {
        onResult: ({ latencyMs }) => {
          this.lastLatencyMs = latencyMs;
          this.pendingLatencyMs = latencyMs;
        },
      });
      this.detachLatencyFrame = this.presented.onFrame(() =>
        this.latencyProbe?.observePresentedFrame(),
      );
      this.latencyChannel = this.connection.getLatencyChannel?.() ?? null;
      if (this.latencyChannel) this.latencyProbe.attach(this.latencyChannel);
    }
    return this.latencyProbe.request();
  }

  teardownResources() {
    this.metricsPump?.stop();
    this.metricsPump = null;
    this.presented?.stop();
    this.presented = null;
    this.latencyProbe?.detach();
    this.latencyProbe = null;
    this.latencyChannel = null;
    this.lastLatencyMs = null;
    this.pendingLatencyMs = null;
    this.detachLatencyFrame?.();
    this.detachLatencyFrame = null;
    this.detachConnectionState?.();
    this.detachConnectionState = null;
    this.detachConnectionProfile?.();
    this.detachConnectionProfile = null;
    this.connection?.close();
    this.connection = null;
    this.provider = null;
    try {
      this.video.pause?.();
    } catch {
      // Teardown does not depend on playback state.
    }
    this.video.srcObject = null;
    this.detachExternalAbort?.();
    this.detachExternalAbort = null;
    this.controller?.abort(new Error("preview closed"));
    this.controller = null;
    this.restarting = false;
    this.staleSamples = 0;
  }

  close({ emit = true } = {}) {
    this.lifecycle += 1;
    this.teardownResources();
    if (emit) this.emitState("closed");
  }
}

function decodeBase64UrlJson(value) {
  if (
    typeof value !== "string" ||
    value.length > 16 * 1024 ||
    !/^[A-Za-z0-9_-]+$/.test(value)
  ) {
    throw new Error("invalid preview attach fragment");
  }
  const padding = "=".repeat((4 - (value.length % 4)) % 4);
  const bytes = Uint8Array.from(atob(value.replaceAll("-", "+").replaceAll("_", "/") + padding),
    (character) => character.charCodeAt(0),
  );
  return JSON.parse(new TextDecoder().decode(bytes));
}

function validatePageConfig(config) {
  if (!config || typeof config !== "object" || Array.isArray(config)) {
    throw new Error("invalid preview attach descriptor");
  }
  if (config.provider !== "webrtc" && config.provider !== "jpeg") {
    throw new Error("unsupported preview provider");
  }
  if (!Object.hasOwn(PREVIEW_PROFILES, config.profile)) {
    throw new Error("unsupported preview profile");
  }
  if (!Object.hasOwn(PRESENTATION_PROFILE, config.presentation)) {
    throw new Error("unsupported preview presentation");
  }
  const allowed = new Set([
    "protocolVersion",
    "sessionId",
    "generation",
    "nonce",
    "token",
    "signaling",
    "profile",
    "provider",
    "presentation",
  ]);
  if (Object.keys(config).some((key) => !allowed.has(key))) {
    throw new Error("unsupported preview descriptor field");
  }
  if (
    !config.signaling ||
    typeof config.signaling !== "object" ||
    Array.isArray(config.signaling) ||
    Object.keys(config.signaling).some((key) => key !== "url")
  ) {
    throw new Error("invalid preview signaling descriptor");
  }
  return config;
}

export function parseAttachFragment(fragment) {
  const parameters = new URLSearchParams(String(fragment).replace(/^#/, ""));
  const entries = [...parameters.entries()];
  if (entries.length !== 1 || entries[0][0] !== "attach") {
    throw new Error("invalid preview attach fragment");
  }
  return validatePageConfig(decodeBase64UrlJson(entries[0][1]));
}

function nativeBridge(payload) {
  const handler = globalThis.webkit?.messageHandlers?.preview;
  if (!handler?.postMessage) return;
  if (payload.type === "state") {
    handler.postMessage({
      type: "state",
      state: payload.state,
      ...(payload.message ? { message: payload.message.slice(0, 512) } : {}),
    });
    return;
  }
  if (payload.type === "metrics") {
    const metrics = payload.metrics;
    handler.postMessage({
      type: "metrics",
      metrics: {
        presentedFps: metrics.presentedFps ?? 0,
        width: metrics.width ?? 0,
        height: metrics.height ?? 0,
        ...(Number.isFinite(metrics.latencyMs) ? { latencyMs: metrics.latencyMs } : {}),
        profile: metrics.profile,
      },
    });
  }
}

/** Installs the strict fragment/native entrypoint used by Safari and WKWebView. */
export function bootstrapPreviewPage({
  root = globalThis.document?.querySelector?.("[data-preview-root]"),
  locationRef = globalThis.location,
  historyRef = globalThis.history,
} = {}) {
  if (!root) throw new Error("preview page root is missing");
  if (!locationRef || !isLoopbackHostname(locationRef.hostname)) {
    throw new Error("preview page must be served from numeric loopback");
  }
  const surface = new PreviewSurface(root, {
    providers: {
      webrtc: () => new WebRtcPreviewProvider({ locationRef }),
      jpeg: () =>
        new JpegPreviewProvider({ frameSource: globalThis.__wscrptJpegFrameSource ?? null }),
    },
    onMessage: nativeBridge,
  });

  const api = Object.freeze({
    attach: async (rawConfig) => {
      const config = validatePageConfig(rawConfig);
      return surface.open({
        session: config,
        provider: config.provider,
        profile: config.profile,
        presentation: config.presentation,
      });
    },
    detach: () => surface.close(),
    setPresentation: (presentation) => surface.setPresentation(presentation),
    requestLatencyProbe: () => surface.requestLatencyProbe(),
  });
  globalThis.wscrptPreview = api;
  globalThis.addEventListener?.("beforeunload", () => surface.close(), { once: true });

  const fragment = String(locationRef.hash ?? "");
  if (fragment) {
    try {
      // Remove the one-use token from the visible URL before starting async work.
      historyRef?.replaceState?.(null, "", `${locationRef.pathname}${locationRef.search}`);
      const config = parseAttachFragment(fragment);
      void api.attach(config).catch(() => {
        // PreviewSurface already emitted a redacted state error.
      });
    } catch {
      historyRef?.replaceState?.(null, "", `${locationRef.pathname}${locationRef.search}`);
      surface.emitState("error", "Invalid attach fragment");
    }
  }
  return { surface, api };
}

if (globalThis.document?.querySelector?.("[data-preview-root]")) {
  bootstrapPreviewPage();
}
