const DEFAULT_SAMPLE_INTERVAL_MS = 1_000;

function monotonicNow() {
  return globalThis.performance?.now?.() ?? Date.now();
}

function finiteOrNull(value) {
  return Number.isFinite(value) ? value : null;
}

function reportValues(report) {
  if (!report) return [];
  if (typeof report.values === "function") return [...report.values()];
  if (typeof report.forEach === "function") {
    const values = [];
    report.forEach((value) => values.push(value));
    return values;
  }
  return Array.isArray(report) ? report : Object.values(report);
}

/** A capacity-one asynchronous handoff. Older generations never replace newer ones. */
export class LatestValue {
  #value;
  #generation = -1;

  publish(value, generation = 0) {
    if (!Number.isSafeInteger(generation) || generation < this.#generation) {
      return false;
    }
    this.#value = value;
    this.#generation = generation;
    return true;
  }

  peek() {
    return this.#value;
  }

  take() {
    const value = this.#value;
    this.#value = undefined;
    return value;
  }

  clear() {
    this.#value = undefined;
    this.#generation = -1;
  }

  get generation() {
    return this.#generation;
  }
}

/** Tracks compositor-presented frames without copying or buffering video frames. */
export class PresentedFrameMetrics {
  constructor(video, { now = monotonicNow } = {}) {
    this.video = video;
    this.now = now;
    this.running = false;
    this.callbackId = null;
    this.presentedFrames = 0;
    this.lastFrameAt = null;
    this.lastSampleAt = this.now();
    this.lastSampleFrames = 0;
    this.maxFreezeMs = 0;
    this.lastMetadata = null;
    this.lastFrameAgeMs = null;
    this.lastFrameAgeBasis = null;
    this.listeners = new Set();
    this.boundFrame = (timestamp, metadata) => this.observeFrame(timestamp, metadata);
  }

  start() {
    if (this.running) return;
    this.running = true;
    if (typeof this.video?.requestVideoFrameCallback === "function") {
      this.callbackId = this.video.requestVideoFrameCallback(this.boundFrame);
    }
  }

  stop() {
    this.running = false;
    if (
      this.callbackId !== null &&
      typeof this.video?.cancelVideoFrameCallback === "function"
    ) {
      this.video.cancelVideoFrameCallback(this.callbackId);
    }
    this.callbackId = null;
    this.listeners.clear();
  }

  onFrame(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  observeFrame(timestamp = this.now(), metadata = {}) {
    if (!this.running && typeof this.video?.requestVideoFrameCallback === "function") {
      return;
    }
    if (this.lastFrameAt !== null) {
      this.maxFreezeMs = Math.max(this.maxFreezeMs, timestamp - this.lastFrameAt);
    }
    this.lastFrameAt = timestamp;
    this.lastMetadata = metadata;
    const captureTime = finiteOrNull(metadata?.captureTime);
    const receiveTime = finiteOrNull(metadata?.receiveTime);
    const sourceTime = captureTime ?? receiveTime;
    const sourceAge = sourceTime === null ? null : timestamp - sourceTime;
    if (Number.isFinite(sourceAge) && sourceAge >= 0 && sourceAge <= 60_000) {
      this.lastFrameAgeMs = sourceAge;
      this.lastFrameAgeBasis = captureTime === null ? "receiveTime" : "captureTime";
    } else {
      this.lastFrameAgeMs = null;
      this.lastFrameAgeBasis = null;
    }
    this.presentedFrames += 1;
    for (const listener of this.listeners) listener({ timestamp, metadata });
    if (this.running && typeof this.video?.requestVideoFrameCallback === "function") {
      this.callbackId = this.video.requestVideoFrameCallback(this.boundFrame);
    }
  }

  sample() {
    const sampledAt = this.now();
    const elapsedMs = Math.max(1, sampledAt - this.lastSampleAt);
    const frameDelta = this.presentedFrames - this.lastSampleFrames;
    const presentationAgeMs =
      this.lastFrameAt === null ? null : Math.max(0, sampledAt - this.lastFrameAt);
    const maxFreezeMs = Math.max(this.maxFreezeMs, presentationAgeMs ?? 0);
    const frameAgeMs =
      this.lastFrameAgeMs === null || presentationAgeMs === null
        ? null
        : this.lastFrameAgeMs + presentationAgeMs;
    const result = {
      sampledAt,
      callbackSupported:
        typeof this.video?.requestVideoFrameCallback === "function",
      presentedFrames: this.presentedFrames,
      presentedFps: (frameDelta * 1_000) / elapsedMs,
      presentationAgeMs,
      frameAgeMs,
      frameAgeBasis: this.lastFrameAgeBasis,
      maxFreezeMs,
      width:
        finiteOrNull(this.lastMetadata?.width) ??
        finiteOrNull(this.video?.videoWidth) ??
        0,
      height:
        finiteOrNull(this.lastMetadata?.height) ??
        finiteOrNull(this.video?.videoHeight) ??
        0,
      mediaTime: finiteOrNull(this.lastMetadata?.mediaTime),
    };
    this.lastSampleAt = sampledAt;
    this.lastSampleFrames = this.presentedFrames;
    // Freeze evidence is interval-scoped so the 10-second warm-up can be
    // excluded without an early gap contaminating every later sample.
    this.maxFreezeMs = 0;
    return result;
  }
}

/**
 * Extracts a compact, log-safe receiver snapshot from getStats(). SDP, ICE
 * candidates, addresses, and certificates are intentionally omitted.
 */
export function extractReceiverStats(report, previous = null, sampledAt = monotonicNow()) {
  const values = reportValues(report);
  const inbound = values.find(
    (entry) =>
      entry.type === "inbound-rtp" &&
      !entry.isRemote &&
      (entry.kind === "video" || entry.mediaType === "video"),
  );
  const transport = values.find((entry) => entry.type === "transport");
  const selectedPairId = transport?.selectedCandidatePairId;
  const candidatePair = values.find(
    (entry) =>
      entry.type === "candidate-pair" &&
      (entry.id === selectedPairId || entry.selected || entry.nominated) &&
      (!entry.state || entry.state === "succeeded"),
  );
  const codec = values.find(
    (entry) => entry.type === "codec" && entry.id === inbound?.codecId,
  );
  const localCandidate = values.find(
    (entry) => entry.type === "local-candidate" && entry.id === candidatePair?.localCandidateId,
  );
  const remoteCandidate = values.find(
    (entry) => entry.type === "remote-candidate" && entry.id === candidatePair?.remoteCandidateId,
  );

  const elapsedSeconds = previous
    ? Math.max(0.001, (sampledAt - previous.sampledAt) / 1_000)
    : null;
  const bytesDelta =
    previous && inbound
      ? Math.max(0, (inbound.bytesReceived ?? 0) - (previous.bytesReceived ?? 0))
      : null;
  const decodedDelta =
    previous && inbound
      ? Math.max(0, (inbound.framesDecoded ?? 0) - (previous.framesDecoded ?? 0))
      : null;

  return {
    sampledAt,
    bytesReceived: inbound?.bytesReceived ?? 0,
    bitrateBps:
      bytesDelta === null ? null : Math.round((bytesDelta * 8) / elapsedSeconds),
    packetsReceived: inbound?.packetsReceived ?? 0,
    packetsReceivedDelta:
      previous && inbound
        ? Math.max(0, (inbound.packetsReceived ?? 0) - (previous.packetsReceived ?? 0))
        : null,
    packetsLost: inbound?.packetsLost ?? 0,
    packetLossDelta:
      previous && inbound
        ? Math.max(0, (inbound.packetsLost ?? 0) - (previous.packetsLost ?? 0))
        : null,
    framesDecoded: inbound?.framesDecoded ?? 0,
    framesDropped: inbound?.framesDropped ?? 0,
    decodedFps:
      finiteOrNull(inbound?.framesPerSecond) ??
      (decodedDelta === null ? null : decodedDelta / elapsedSeconds),
    width: inbound?.frameWidth ?? 0,
    height: inbound?.frameHeight ?? 0,
    jitterSeconds: finiteOrNull(inbound?.jitter),
    rttMs: Number.isFinite(candidatePair?.currentRoundTripTime)
      ? candidatePair.currentRoundTripTime * 1_000
      : null,
    availableIncomingBitrate: finiteOrNull(candidatePair?.availableIncomingBitrate),
    codec: codec?.mimeType ?? null,
    codecPayloadType: codec?.payloadType ?? null,
    localCandidateType:
      typeof localCandidate?.candidateType === "string" ? localCandidate.candidateType : null,
    remoteCandidateType:
      typeof remoteCandidate?.candidateType === "string" ? remoteCandidate.candidateType : null,
  };
}

/** A one-at-a-time, test-only request-to-glass probe for the clock fixture. */
export class FixtureLatencyProbe {
  constructor(
    video,
    {
      now = monotonicNow,
      createCanvas = () => globalThis.document?.createElement("canvas"),
      onResult = () => {},
      tolerance = 55,
      timeoutMs = 5_000,
      setTimeoutFn = globalThis.setTimeout?.bind(globalThis),
      clearTimeoutFn = globalThis.clearTimeout?.bind(globalThis),
    } = {},
  ) {
    this.video = video;
    this.now = now;
    this.onResult = onResult;
    this.tolerance = tolerance;
    this.timeoutMs = timeoutMs;
    this.setTimeoutFn = setTimeoutFn;
    this.clearTimeoutFn = clearTimeoutFn;
    this.channel = null;
    this.pending = null;
    this.timeout = null;
    this.sequence = 0;
    this.canvas = createCanvas();
    if (this.canvas) {
      this.canvas.width = 1;
      this.canvas.height = 1;
      this.context = this.canvas.getContext("2d", { willReadFrequently: true });
    }
    this.handleMessage = (event) => this.#onMessage(event);
  }

  attach(channel) {
    this.detach();
    this.channel = channel;
    channel?.addEventListener?.("message", this.handleMessage);
  }

  detach() {
    this.channel?.removeEventListener?.("message", this.handleMessage);
    this.channel = null;
    if (this.timeout !== null) this.clearTimeoutFn?.(this.timeout);
    this.timeout = null;
    this.pending = null;
  }

  request() {
    if (this.pending) throw new Error("a latency probe is already outstanding");
    if (!this.channel || this.channel.readyState !== "open") {
      throw new Error("latency data channel is not open");
    }
    const id = `probe-${++this.sequence}`;
    const rgb = this.sequence % 2 === 0 ? [0, 255, 255] : [255, 0, 255];
    this.pending = { id, rgb, requestedAt: this.now(), armed: false };
    if (this.setTimeoutFn) {
      this.timeout = this.setTimeoutFn(() => {
        if (this.pending?.id === id) this.pending = null;
        this.timeout = null;
      }, this.timeoutMs);
    }
    try {
      this.channel.send(JSON.stringify({ type: "latency-probe", id, rgb }));
    } catch (error) {
      if (this.timeout !== null) this.clearTimeoutFn?.(this.timeout);
      this.timeout = null;
      this.pending = null;
      throw error;
    }
    return id;
  }

  observePresentedFrame() {
    if (!this.pending?.armed || !this.context) return null;
    try {
      this.context.drawImage(this.video, 0, 0, 1, 1, 0, 0, 1, 1);
      const pixel = this.context.getImageData(0, 0, 1, 1).data;
      const matches = this.pending.rgb.every(
        (component, index) => Math.abs(component - pixel[index]) <= this.tolerance,
      );
      if (!matches) return null;
      const result = {
        id: this.pending.id,
        latencyMs: Math.max(0, this.now() - this.pending.requestedAt),
      };
      if (this.timeout !== null) this.clearTimeoutFn?.(this.timeout);
      this.timeout = null;
      this.pending = null;
      this.onResult(result);
      return result;
    } catch {
      return null;
    }
  }

  #onMessage(event) {
    if (!this.pending) return;
    let message;
    try {
      message = JSON.parse(String(event.data));
    } catch {
      return;
    }
    if (message.type === "latency-flash-armed" && message.id === this.pending.id) {
      this.pending.armed = true;
    }
  }
}

/** Samples at 1 Hz with at most one getStats() call in flight. */
export class MetricsPump {
  constructor(
    { connection, presented, profile, generation, onSample },
    {
      intervalMs = DEFAULT_SAMPLE_INTERVAL_MS,
      setIntervalFn = globalThis.setInterval?.bind(globalThis),
      clearIntervalFn = globalThis.clearInterval?.bind(globalThis),
    } = {},
  ) {
    this.connection = connection;
    this.presented = presented;
    this.profile = profile;
    this.generation = generation;
    this.onSample = onSample;
    this.intervalMs = intervalMs;
    this.setIntervalFn = setIntervalFn;
    this.clearIntervalFn = clearIntervalFn;
    this.timer = null;
    this.inFlight = false;
    this.latest = new LatestValue();
  }

  start() {
    if (this.timer !== null || !this.setIntervalFn) return;
    this.timer = this.setIntervalFn(() => void this.sample(), this.intervalMs);
  }

  async sample() {
    if (this.inFlight) return this.latest.peek();
    this.inFlight = true;
    try {
      const [transport, frame] = await Promise.all([
        this.connection.sampleStats(),
        Promise.resolve(this.presented.sample()),
      ]);
      const value = {
        type: "metrics",
        sampledAt: Date.now(),
        profile: typeof this.profile === "function" ? this.profile() : this.profile,
        generation:
          typeof this.generation === "function" ? this.generation() : this.generation,
        ...transport,
        ...frame,
      };
      this.latest.publish(value, value.generation ?? 0);
      this.onSample?.(value);
      return value;
    } finally {
      this.inFlight = false;
    }
  }

  stop() {
    if (this.timer !== null && this.clearIntervalFn) this.clearIntervalFn(this.timer);
    this.timer = null;
  }
}
