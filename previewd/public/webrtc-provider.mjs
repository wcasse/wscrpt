import { extractReceiverStats } from "./metrics.mjs";

export const PREVIEW_PROFILES = Object.freeze({
  mini: Object.freeze({ width: 960, height: 540, fps: 24, maxBitrate: 4_000_000 }),
  expanded: Object.freeze({
    width: 1280,
    height: 720,
    fps: 24,
    maxBitrate: 6_000_000,
  }),
  "expanded-headroom": Object.freeze({
    width: 1280,
    height: 720,
    fps: 30,
    maxBitrate: 8_000_000,
  }),
  fallback: Object.freeze({
    width: 960,
    height: 540,
    fps: 12,
    maxBitrate: 1_500_000,
  }),
});

const PROTOCOL_VERSION = 1;
const MAX_SIGNAL_BYTES = 64 * 1024;
const MAX_PENDING_CANDIDATES = 32;
const DEFAULT_MAX_INBOUND_SIGNALS = 32;
const DEFAULT_MAX_BUFFERED_SIGNAL_BYTES = 256 * 1024;
const IDENTIFIER = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u;
const NONCE = /^[A-Za-z0-9_-]{16,128}$/u;
const TOKEN = /^[A-Za-z0-9_-]{32,256}$/u;

function utf8Length(value) {
  return new TextEncoder().encode(value).byteLength;
}

function websocketOpen(socket) {
  return socket?.readyState === 1;
}

function addListener(target, type, listener, options) {
  if (typeof target?.addEventListener === "function") {
    target.addEventListener(type, listener, options);
    return () => target.removeEventListener?.(type, listener, options);
  }
  const property = `on${type}`;
  const previous = target?.[property];
  if (target) target[property] = listener;
  return () => {
    if (target?.[property] === listener) target[property] = previous ?? null;
  };
}

export function isLoopbackHostname(hostname) {
  const host = String(hostname).toLowerCase().replace(/^\[|\]$/g, "");
  if (host === "::1") return true;
  const octets = host.split(".");
  if (octets.length !== 4 || octets.some((part) => !/^\d{1,3}$/.test(part))) {
    return false;
  }
  return (
    Number(octets[0]) === 127 &&
    octets.every((part) => Number(part) >= 0 && Number(part) <= 255)
  );
}

export function assertLoopbackWebSocketUrl(value) {
  const url = value instanceof URL ? value : new URL(String(value));
  if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw new Error("preview signaling must use ws or wss");
  }
  if (!isLoopbackHostname(url.hostname)) {
    throw new Error("preview signaling must use a numeric loopback host");
  }
  if (url.username || url.password || url.search || url.hash) {
    throw new Error("preview signaling URL must not contain credentials, query, or fragment");
  }
  if (url.pathname !== "/signal") {
    throw new Error("preview signaling path must be /signal");
  }
  if (!url.port) throw new Error("preview signaling requires an explicit forwarded port");
  return url;
}

export function resolveSignalingUrl(session, locationRef = globalThis.location) {
  const explicit = session?.signaling?.url ?? session?.signalUrl;
  if (explicit) {
    const url = assertLoopbackWebSocketUrl(explicit);
    if (locationRef) {
      if (!isLoopbackHostname(locationRef.hostname)) {
        throw new Error("receiver page must be served from numeric loopback");
      }
      const pageHost = String(locationRef.hostname).toLowerCase().replace(/^\[|\]$/g, "");
      const signalHost = url.hostname.toLowerCase().replace(/^\[|\]$/g, "");
      if (pageHost !== signalHost || String(locationRef.port) !== url.port) {
        throw new Error("preview signaling must use the player page loopback endpoint");
      }
    }
    return url;
  }
  if (!locationRef) throw new Error("missing loopback signaling URL");
  if (!isLoopbackHostname(locationRef.hostname)) {
    throw new Error("receiver page must be served from numeric loopback");
  }
  const port = session?.localPort ?? locationRef.port;
  if (!/^\d+$/.test(String(port)) || Number(port) < 1 || Number(port) > 65535) {
    throw new Error("invalid forwarded signaling port");
  }
  const scheme = locationRef.protocol === "https:" ? "wss:" : "ws:";
  const host = locationRef.hostname.includes(":")
    ? `[${locationRef.hostname.replace(/^\[|\]$/g, "")}]`
    : locationRef.hostname;
  return assertLoopbackWebSocketUrl(`${scheme}//${host}:${port}/signal`);
}

export function validateReceiverSession(session) {
  if (!session || typeof session !== "object" || Array.isArray(session)) {
    throw new Error("preview session must be an object");
  }
  if (session.protocolVersion !== PROTOCOL_VERSION) {
    throw new Error("unsupported preview protocol version");
  }
  if (!IDENTIFIER.test(session.sessionId ?? "")) throw new Error("invalid preview session sessionId");
  if (!NONCE.test(session.nonce ?? "")) throw new Error("invalid preview session nonce");
  if (!TOKEN.test(session.token ?? "")) throw new Error("invalid preview session token");
  if (!Number.isSafeInteger(session.generation) || session.generation < 1) {
    throw new Error("invalid preview generation");
  }
  return session;
}

function validateProfile(profile) {
  if (!Object.hasOwn(PREVIEW_PROFILES, profile)) {
    throw new Error(`unknown preview profile: ${profile}`);
  }
  return profile;
}

function orderVideoCodecs(transceiver) {
  if (typeof transceiver?.setCodecPreferences !== "function") return;
  const codecs = globalThis.RTCRtpReceiver?.getCapabilities?.("video")?.codecs;
  if (!Array.isArray(codecs) || codecs.length === 0) return;
  const h264 = codecs.filter((codec) => /video\/H264/i.test(codec.mimeType));
  const rest = codecs.filter((codec) => !/video\/H264/i.test(codec.mimeType));
  try {
    transceiver.setCodecPreferences([...h264, ...rest]);
  } catch {
    // Negotiation still works with the browser's default codec preference.
  }
}

class ReceiverConnection {
  constructor(provider, session, profile, signal, signalUrl) {
    this.provider = provider;
    this.session = { ...session };
    this.profile = validateProfile(profile);
    this.generation = session.generation;
    this.signal = signal;
    this.signalUrl = signalUrl;
    this.socket = null;
    this.peer = null;
    this.closed = false;
    this.joined = false;
    this.pendingCandidates = [];
    this.remoteDescriptionReady = false;
    this.previousStats = null;
    this.currentState = { state: "connecting" };
    this.offerEpoch = 0;
    this.inboundChain = Promise.resolve();
    this.queuedInboundSignals = 0;
    this.stateListeners = new Set();
    this.profileListeners = new Set();
    this.cleanupListeners = [];
    this.latencyChannel = null;
    this.stream = new provider.MediaStreamClass();
  }

  envelope(type, payload = {}) {
    return {
      protocolVersion: PROTOCOL_VERSION,
      sessionId: this.session.sessionId,
      generation: this.generation,
      nonce: this.session.nonce,
      type,
      ...payload,
    };
  }

  emitState(state, message) {
    const detail = { state, ...(message ? { message } : {}) };
    this.currentState = detail;
    for (const listener of this.stateListeners) listener(detail);
  }

  onState(listener) {
    this.stateListeners.add(listener);
    listener(this.currentState);
    return () => this.stateListeners.delete(listener);
  }

  onProfile(listener) {
    this.profileListeners.add(listener);
    return () => this.profileListeners.delete(listener);
  }

  applyProfile(profile) {
    this.profile = validateProfile(profile);
    for (const listener of this.profileListeners) listener(this.profile);
  }

  send(type, payload = {}) {
    if (!websocketOpen(this.socket)) throw new Error("preview signaling is not open");
    const encoded = JSON.stringify(this.envelope(type, payload));
    const size = utf8Length(encoded);
    if (size > MAX_SIGNAL_BYTES) {
      throw new Error("preview signaling message exceeds 64 KiB");
    }
    const bufferedAmount = Number.isFinite(this.socket.bufferedAmount)
      ? this.socket.bufferedAmount
      : 0;
    if (bufferedAmount + size > this.provider.maxBufferedSignalBytes) {
      const error = new Error("preview signaling outbound buffer limit exceeded");
      this.failSignaling(error);
      throw error;
    }
    this.socket.send(encoded);
  }

  failSignaling(error) {
    if (this.closed) return;
    this._rejectJoin?.(error);
    this.emitState("error", error instanceof Error ? error.message : String(error));
    this.close({ sendLeave: false });
  }

  enqueueMessage(raw) {
    if (this.closed) return;
    if (typeof raw !== "string") {
      this.failSignaling(new Error("preview signaling requires text messages"));
      return;
    }
    if (utf8Length(raw) > MAX_SIGNAL_BYTES) {
      this.failSignaling(new Error("preview signaling message exceeds 64 KiB"));
      return;
    }
    if (this.queuedInboundSignals >= this.provider.maxInboundSignals) {
      this.failSignaling(new Error("preview signaling inbound queue limit exceeded"));
      return;
    }
    this.queuedInboundSignals += 1;
    this.inboundChain = this.inboundChain
      .then(() => this.handleMessage(raw))
      .catch((error) => this.failSignaling(error))
      .finally(() => {
        this.queuedInboundSignals -= 1;
      });
  }

  async start() {
    if (this.signal?.aborted) throw this.signal.reason ?? new Error("preview attach aborted");
    this.emitState("connecting");
    this.socket = new this.provider.WebSocketClass(this.signalUrl.href);
    this.socket.binaryType = "arraybuffer";

    const ready = new Promise((resolve, reject) => {
      const timeout = this.provider.setTimeoutFn(
        () => reject(new Error("preview signaling join timed out")),
        this.provider.joinTimeoutMs,
      );
      const settle = (callback) => (value) => {
        this.provider.clearTimeoutFn(timeout);
        callback(value);
      };
      this._resolveJoin = settle(resolve);
      this._rejectJoin = settle(reject);
    });

    this.cleanupListeners.push(
      addListener(this.socket, "open", () => {
        try {
          const token = this.session.token;
          this.send("join", {
            role: "receiver",
            token,
            profile: this.profile,
          });
          delete this.session.token;
        } catch (error) {
          this._rejectJoin?.(error);
        }
      }),
      addListener(this.socket, "message", (event) => this.enqueueMessage(event.data)),
      addListener(this.socket, "error", () => {
        const error = new Error("preview signaling connection failed");
        this.failSignaling(error);
      }),
      addListener(this.socket, "close", () => {
        if (!this.closed) {
          const error = new Error("preview signaling connection closed");
          this.failSignaling(error);
        }
      }),
    );
    if (this.signal) {
      const abort = () => this.close();
      this.signal.addEventListener("abort", abort, { once: true });
      this.cleanupListeners.push(() => this.signal.removeEventListener("abort", abort));
    }

    try {
      await ready;
      return this.publicHandle();
    } catch (error) {
      this.close();
      throw error;
    }
  }

  async handleMessage(raw) {
    if (this.closed || typeof raw !== "string" || utf8Length(raw) > MAX_SIGNAL_BYTES) {
      return;
    }
    let message;
    try {
      message = JSON.parse(raw);
    } catch {
      return;
    }
    if (
      message?.protocolVersion !== PROTOCOL_VERSION ||
      message.sessionId !== this.session.sessionId ||
      message.nonce !== this.session.nonce ||
      !Number.isSafeInteger(message.generation)
    ) {
      return;
    }
    if (message.type !== "joined" && message.generation !== this.generation) return;

    try {
      switch (message.type) {
        case "joined":
          if (message.generation < this.generation) return;
          this.generation = message.generation;
          this.joined = true;
          if (this.currentState.state !== "playing") this.emitState("connecting");
          this._resolveJoin?.();
          this._resolveJoin = null;
          this._rejectJoin = null;
          break;
        case "answer":
          await this.acceptAnswer(message.description);
          break;
        case "offer":
          await this.acceptOffer(message.description);
          break;
        case "ice":
          await this.acceptCandidate(message.candidate);
          break;
        case "state":
          if (message.profile !== undefined) this.applyProfile(message.profile);
          if (message.state === "restart-required") {
            await this.restart(message.reason ?? "stale-frame-age");
          } else if (["failed", "disconnected", "closed"].includes(message.state)) {
            this.emitState(message.state, message.message);
          } else if (
            this.currentState.state !== "playing" &&
            ["new", "connecting", "ready"].includes(message.state)
          ) {
            // Source lifecycle is supporting detail. The receiver's ontrack and
            // peer state are authoritative once media is playing; profile
            // acknowledgements must never downgrade a live surface.
            this.emitState("connecting", message.message);
          }
          break;
        case "profile":
          this.applyProfile(message.profile);
          break;
        case "error": {
          const reason =
            typeof message.message === "string" ? message.message : "remote preview error";
          this._rejectJoin?.(new Error(reason));
          this.emitState("error", reason);
          break;
        }
        case "leave":
          this.emitState("error", "remote preview sender left");
          break;
        default:
          break;
      }
    } catch (error) {
      this._rejectJoin?.(error);
      this.emitState("error", error instanceof Error ? error.message : String(error));
    }
  }

  removeRemoteTracks() {
    for (const track of this.stream.getTracks()) {
      this.stream.removeTrack?.(track);
      track.stop?.();
    }
  }

  closePeer() {
    this.offerEpoch += 1;
    this.pendingCandidates = [];
    this.remoteDescriptionReady = false;
    if (this.latencyChannel) {
      try {
        this.latencyChannel.close();
      } catch {
        // Closing is best effort.
      }
    }
    this.latencyChannel = null;
    if (this.peer) {
      this.peer.ontrack = null;
      this.peer.onicecandidate = null;
      this.peer.onconnectionstatechange = null;
      try {
        this.peer.close();
      } catch {
        // Closing is idempotent at this boundary.
      }
    }
    this.peer = null;
    this.removeRemoteTracks();
    this.previousStats = null;
  }

  createPeer() {
    this.closePeer();
    if (this.closed) return null;
    const peer = new this.provider.RTCPeerConnectionClass({ iceServers: [] });
    this.peer = peer;
    peer.onicecandidate = (event) => {
      if (this.peer !== peer || !event.candidate) return;
      try {
        this.send("ice", {
          candidate:
            typeof event.candidate.toJSON === "function"
              ? event.candidate.toJSON()
              : event.candidate,
        });
      } catch (error) {
        this.emitState("error", error.message);
      }
    };
    peer.ontrack = (event) => {
      if (this.peer !== peer || !event.track) return;
      for (const track of this.stream.getTracks()) {
        if (track.kind === event.track.kind && track.id !== event.track.id) {
          this.stream.removeTrack?.(track);
          track.stop?.();
        }
      }
      if (!this.stream.getTracks().some((track) => track.id === event.track.id)) {
        this.stream.addTrack(event.track);
      }
      this.emitState("playing");
    };
    peer.onconnectionstatechange = () => {
      if (this.peer !== peer) return;
      const state = peer.connectionState;
      if (state === "connected") this.emitState("playing");
      if (state === "failed" || state === "disconnected") {
        this.emitState("error", `WebRTC ${state}`);
      }
    };
    peer.ondatachannel = (event) => {
      if (
        this.peer !== peer ||
        event.channel?.label !== "wscrpt-latency"
      ) {
        event.channel?.close?.();
        return;
      }
      this.latencyChannel?.close?.();
      this.latencyChannel = event.channel;
      this.provider.onDataChannel?.(event.channel);
    };
    return peer;
  }

  async acceptAnswer(description) {
    if (!this.peer || !description || description.type !== "answer") return;
    const peer = this.peer;
    const epoch = this.offerEpoch;
    await peer.setRemoteDescription(description);
    if (this.peer !== peer || this.offerEpoch !== epoch || this.closed) return;
    this.remoteDescriptionReady = true;
    await this.flushCandidates(peer, epoch);
  }

  async acceptOffer(description) {
    if (!description || description.type !== "offer") return;
    const peer = this.createPeer();
    if (!peer) return;
    const epoch = this.offerEpoch;
    await peer.setRemoteDescription(description);
    if (this.peer !== peer || this.offerEpoch !== epoch || this.closed) return;
    for (const transceiver of peer.getTransceivers?.() ?? []) orderVideoCodecs(transceiver);
    this.remoteDescriptionReady = true;
    await this.flushCandidates(peer, epoch);
    if (this.peer !== peer || this.offerEpoch !== epoch || this.closed) return;
    const answer = await peer.createAnswer();
    if (this.peer !== peer || this.offerEpoch !== epoch || this.closed) return;
    await peer.setLocalDescription(answer);
    if (this.peer !== peer || this.offerEpoch !== epoch || this.closed) return;
    this.send("answer", { description: peer.localDescription ?? answer });
  }

  async acceptCandidate(candidate) {
    if (!this.peer || candidate === undefined) return;
    if (!this.remoteDescriptionReady) {
      if (this.pendingCandidates.length >= MAX_PENDING_CANDIDATES) {
        throw new Error("too many pending ICE candidates");
      }
      this.pendingCandidates.push(candidate);
      return;
    }
    await this.peer.addIceCandidate(candidate);
  }

  async flushCandidates(peer = this.peer, epoch = this.offerEpoch) {
    const pending = this.pendingCandidates;
    this.pendingCandidates = [];
    for (const candidate of pending) {
      if (this.peer !== peer || this.offerEpoch !== epoch || this.closed) return;
      await peer?.addIceCandidate(candidate);
    }
  }

  async setProfile(profile) {
    this.profile = validateProfile(profile);
    this.send("profile", { profile: this.profile });
  }

  reportStats(stats) {
    if (!stats || typeof stats !== "object") return;
    this.send("stats", { stats });
  }

  async restart(reason = "stale-frame-age") {
    if (this.closed || !this.joined) return;
    this.generation += 1;
    this.send("state", { state: "restarting", reason });
    this.closePeer();
    this.send("join", {
      role: "receiver",
      profile: this.profile,
    });
  }

  async sampleStats() {
    if (!this.peer || typeof this.peer.getStats !== "function") {
      return { profile: this.profile, generation: this.generation };
    }
    const report = await this.peer.getStats();
    const sampledAt = this.provider.now();
    const stats = extractReceiverStats(report, this.previousStats, sampledAt);
    this.previousStats = stats;
    return { ...stats, profile: this.profile, generation: this.generation };
  }

  close({ sendLeave = true } = {}) {
    if (this.closed) return;
    this._rejectJoin?.(new Error("preview signaling closed before join"));
    this._resolveJoin = null;
    this._rejectJoin = null;
    if (sendLeave && websocketOpen(this.socket)) {
      try {
        this.send("leave", { reason: "receiver-close" });
      } catch {
        // Teardown continues even if signaling is already failing.
      }
    }
    this.closed = true;
    this.closePeer();
    for (const cleanup of this.cleanupListeners.splice(0)) cleanup();
    try {
      this.socket?.close(1000, "receiver-close");
    } catch {
      // Teardown is best effort.
    }
    this.socket = null;
    this.stateListeners.clear();
    this.profileListeners.clear();
  }

  publicHandle() {
    return Object.freeze({
      stream: this.stream,
      setProfile: (profile) => this.setProfile(profile),
      sampleStats: () => this.sampleStats(),
      restart: (reason) => this.restart(reason),
      close: () => this.close(),
      onState: (listener) => this.onState(listener),
      onProfile: (listener) => this.onProfile(listener),
      reportStats: (stats) => this.reportStats(stats),
      getGeneration: () => this.generation,
      getLatencyChannel: () => this.latencyChannel,
    });
  }
}

export class WebRtcPreviewProvider {
  constructor({
    WebSocketClass = globalThis.WebSocket,
    RTCPeerConnectionClass = globalThis.RTCPeerConnection,
    MediaStreamClass = globalThis.MediaStream,
    locationRef = globalThis.location,
    now = () => globalThis.performance?.now?.() ?? Date.now(),
    joinTimeoutMs = 10_000,
    maxInboundSignals = DEFAULT_MAX_INBOUND_SIGNALS,
    maxBufferedSignalBytes = DEFAULT_MAX_BUFFERED_SIGNAL_BYTES,
    setTimeoutFn = globalThis.setTimeout?.bind(globalThis),
    clearTimeoutFn = globalThis.clearTimeout?.bind(globalThis),
    onDataChannel = null,
  } = {}) {
    if (!WebSocketClass || !RTCPeerConnectionClass || !MediaStreamClass) {
      throw new Error("browser WebRTC APIs are unavailable");
    }
    if (!Number.isSafeInteger(maxInboundSignals) || maxInboundSignals < 1 || maxInboundSignals > 256) {
      throw new Error("maxInboundSignals must be between 1 and 256");
    }
    if (
      !Number.isSafeInteger(maxBufferedSignalBytes) ||
      maxBufferedSignalBytes < MAX_SIGNAL_BYTES ||
      maxBufferedSignalBytes > 4 * 1024 * 1024
    ) {
      throw new Error("maxBufferedSignalBytes must be between 64 KiB and 4 MiB");
    }
    this.WebSocketClass = WebSocketClass;
    this.RTCPeerConnectionClass = RTCPeerConnectionClass;
    this.MediaStreamClass = MediaStreamClass;
    this.locationRef = locationRef;
    this.now = now;
    this.joinTimeoutMs = joinTimeoutMs;
    this.maxInboundSignals = maxInboundSignals;
    this.maxBufferedSignalBytes = maxBufferedSignalBytes;
    this.setTimeoutFn = setTimeoutFn;
    this.clearTimeoutFn = clearTimeoutFn;
    this.onDataChannel = onDataChannel;
  }

  async connect({ session, profile = "mini", signal } = {}) {
    validateReceiverSession(session);
    validateProfile(profile);
    const signalUrl = resolveSignalingUrl(session, this.locationRef);
    const connection = new ReceiverConnection(this, session, profile, signal, signalUrl);
    return connection.start();
  }
}
