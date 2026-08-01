(() => {
  "use strict";

  const root = globalThis;
  const VERSION = 1;
  const BINDING_NAME = "__wscrptPreviewBridge";
  const MAX_MESSAGE_BYTES = 64 * 1024;
  const MAX_PENDING_CANDIDATES = 32;
  const FIXTURE_FLASH_REQUEST_EVENT = "wscrpt-preview-fixture-flash-v1";
  const FIXTURE_FLASH_RESULT_EVENT = "wscrpt-preview-fixture-flash-result-v1";
  const FIXTURE_FLASH_TIMEOUT_MS = 1_000;
  const IDENTIFIER = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u;
  const NONCE = /^[A-Za-z0-9_-]{16,128}$/u;
  const FIXTURE_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u;
  const FIXTURE_REQUEST_ID = /^[a-f0-9]{32}$/u;
  const PROFILES = Object.freeze({
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

  if (root.__wscrptPreviewSender?.version === VERSION) return;
  root.__wscrptPreviewSender?.stop?.({ reason: "sender-version-replaced" });

  let session = null;

  function encodedLength(value) {
    return new TextEncoder().encode(value).byteLength;
  }

  function requireText(value, name, maximum = 512) {
    if (typeof value !== "string" || !value || value.length > maximum) {
      throw new Error(`invalid ${name}`);
    }
    return value;
  }

  function requireProfile(profile) {
    if (!Object.hasOwn(PROFILES, profile)) throw new Error(`unknown profile: ${profile}`);
    return profile;
  }

  function validateBootstrap(config) {
    if (!config || typeof config !== "object" || config.protocolVersion !== VERSION) {
      throw new Error("unsupported preview sender configuration");
    }
    if (!IDENTIFIER.test(config.sessionId ?? "")) throw new Error("invalid sessionId");
    if (!NONCE.test(config.nonce ?? "")) throw new Error("invalid nonce");
    requireText(config.canvasSelector, "canvasSelector", 2_048);
    if (!Number.isSafeInteger(config.generation) || config.generation < 1) {
      throw new Error("invalid generation");
    }
    requireProfile(config.profile ?? "mini");
    return config;
  }

  function resolveExactCanvas(selector) {
    let matches;
    try {
      matches = root.document?.querySelectorAll(selector);
    } catch (error) {
      throw new Error(`invalid canvas selector: ${error.message}`);
    }
    if (!matches || matches.length !== 1) {
      throw new Error(`canvas selector matched ${matches?.length ?? 0} elements`);
    }
    const canvas = matches[0];
    if (root.HTMLCanvasElement && !(canvas instanceof root.HTMLCanvasElement)) {
      throw new Error("selected element is not an HTML canvas");
    }
    if (typeof canvas.captureStream !== "function") {
      throw new Error("canvas.captureStream is unavailable");
    }
    if (!Number.isFinite(canvas.width) || !Number.isFinite(canvas.height)) {
      throw new Error("canvas has invalid dimensions");
    }
    return canvas;
  }

  function envelope(type, payload = {}) {
    return {
      protocolVersion: VERSION,
      sessionId: session.config.sessionId,
      generation: session.generation,
      nonce: session.config.nonce,
      type,
      ...payload,
    };
  }

  function emit(type, payload = {}) {
    if (!session) return;
    const binding = root[BINDING_NAME];
    if (typeof binding !== "function") throw new Error("preview CDP binding is unavailable");
    const encoded = JSON.stringify(envelope(type, payload));
    if (encodedLength(encoded) > MAX_MESSAGE_BYTES) {
      throw new Error("preview signal exceeds 64 KiB");
    }
    binding(encoded);
  }

  function emitError(error, code = "sender-error") {
    try {
      emit("error", {
        code,
        message: error instanceof Error ? error.message : String(error),
      });
    } catch {
      // There is no safe secondary channel if the fixed binding itself failed.
    }
  }

  function compactCandidate(candidate) {
    if (!candidate) return null;
    if (typeof candidate.toJSON === "function") return candidate.toJSON();
    return {
      candidate: candidate.candidate,
      sdpMid: candidate.sdpMid,
      sdpMLineIndex: candidate.sdpMLineIndex,
      usernameFragment: candidate.usernameFragment,
    };
  }

  function stopCapture(capture) {
    if (!capture) return;
    capture.cancel?.();
    if (capture.rafId !== null) root.cancelAnimationFrame?.(capture.rafId);
    for (const track of capture.stream?.getTracks?.() ?? []) track.stop?.();
  }

  function closePeer() {
    if (!session) return;
    if (session.statsTimer !== null) root.clearInterval(session.statsTimer);
    session.statsTimer = null;
    session.statsBusy = false;
    session.fixtureFlashCancel?.();
    session.fixtureFlashCancel = null;
    try {
      session.dataChannel?.close?.();
    } catch {
      // Teardown remains best effort.
    }
    session.dataChannel = null;
    stopCapture(session.capture);
    session.capture = null;
    if (session.peer) {
      session.peer.onicecandidate = null;
      session.peer.onconnectionstatechange = null;
      try {
        session.peer.close();
      } catch {
        // Teardown is idempotent at this boundary.
      }
    }
    session.peer = null;
    session.sender = null;
    session.pendingCandidates = [];
    session.remoteDescriptionReady = false;
    session.previousStats = null;
  }

  function createMirrorCapture(source, quality) {
    const mirror = root.document.createElement("canvas");
    mirror.width = quality.width;
    mirror.height = quality.height;
    const context = mirror.getContext("2d", { alpha: false, desynchronized: true });
    if (!context) throw new Error("unable to create bounded mirror canvas");
    let stopped = false;
    const capture = { stream: null, track: null, mirror, rafId: null, direct: false };
    const draw = () => {
      if (stopped) return;
      context.drawImage(source, 0, 0, mirror.width, mirror.height);
      capture.rafId = root.requestAnimationFrame(draw);
    };
    draw();
    capture.stream = mirror.captureStream(quality.fps);
    capture.track = capture.stream.getVideoTracks()[0];
    capture.cancel = () => {
      stopped = true;
      if (capture.rafId !== null) root.cancelAnimationFrame?.(capture.rafId);
    };
    return capture;
  }

  function createCapture(profile, forceMirror = false) {
    const quality = PROFILES[profile];
    const canvas = resolveExactCanvas(session.config.canvasSelector);
    if (canvas.width < quality.width || canvas.height < quality.height) {
      throw new Error(
        `source canvas ${canvas.width}x${canvas.height} is smaller than ${quality.width}x${quality.height}`,
      );
    }
    const xScale = canvas.width / quality.width;
    const yScale = canvas.height / quality.height;
    if (Math.abs(xScale - yScale) > 0.01) {
      throw new Error("source and target aspect ratios differ");
    }
    if (forceMirror) return createMirrorCapture(canvas, quality);
    const stream = canvas.captureStream(quality.fps);
    const track = stream.getVideoTracks()[0];
    if (!track) throw new Error("canvas capture produced no video track");
    try {
      track.contentHint = "detail";
    } catch {
      // contentHint is advisory.
    }
    return { stream, track, mirror: null, rafId: null, direct: true, scale: xScale };
  }

  async function configureSender(sender, profile, capture) {
    const quality = PROFILES[profile];
    if (typeof sender.setParameters !== "function") {
      return capture.scale === 1;
    }
    const parameters = sender.getParameters?.() ?? {};
    if (!Array.isArray(parameters.encodings) || parameters.encodings.length === 0) {
      parameters.encodings = [{}];
    }
    parameters.encodings[0] = {
      ...parameters.encodings[0],
      maxBitrate: quality.maxBitrate,
      maxFramerate: quality.fps,
      scaleResolutionDownBy: capture.direct ? capture.scale : 1,
    };
    parameters.degradationPreference = "maintain-resolution";
    try {
      await sender.setParameters(parameters);
      return true;
    } catch {
      return capture.scale === 1;
    }
  }

  function preferH264(transceiver) {
    if (typeof transceiver?.setCodecPreferences !== "function") return;
    const codecs = root.RTCRtpSender?.getCapabilities?.("video")?.codecs;
    if (!Array.isArray(codecs) || codecs.length === 0) return;
    const h264 = codecs.filter((codec) => /video\/H264/i.test(codec.mimeType));
    const rest = codecs.filter((codec) => !/video\/H264/i.test(codec.mimeType));
    try {
      transceiver.setCodecPreferences([...h264, ...rest]);
    } catch {
      // The browser default remains a valid Phase 0 negotiation path.
    }
  }

  function fixtureRequestId() {
    const bytes = new Uint8Array(16);
    root.crypto?.getRandomValues?.(bytes);
    if (bytes.every((value) => value === 0)) {
      throw new Error("secure fixture request correlation is unavailable");
    }
    return [...bytes].map((value) => value.toString(16).padStart(2, "0")).join("");
  }

  function requestFixtureFlash(id, rgb) {
    if (!session || session.config.fixtureLatency !== true) return Promise.resolve(null);
    const documentRef = root.document;
    if (
      !documentRef?.addEventListener ||
      !documentRef?.removeEventListener ||
      !documentRef?.dispatchEvent ||
      typeof root.CustomEvent !== "function"
    ) {
      return Promise.resolve(null);
    }
    session.fixtureFlashCancel?.();
    const requestId = fixtureRequestId();
    return new Promise((resolvePromise) => {
      let timeout = null;
      let settled = false;
      const finish = (result) => {
        if (settled) return;
        settled = true;
        if (timeout !== null) root.clearTimeout(timeout);
        documentRef.removeEventListener(FIXTURE_FLASH_RESULT_EVENT, onResult);
        if (session?.fixtureFlashCancel === cancel) session.fixtureFlashCancel = null;
        resolvePromise(result);
      };
      const cancel = () => finish(null);
      const onResult = (event) => {
        if (typeof event?.detail !== "string" || event.detail.length > 512) return;
        let result;
        try {
          result = JSON.parse(event.detail);
        } catch {
          return;
        }
        if (
          !result ||
          typeof result !== "object" ||
          Array.isArray(result) ||
          Object.keys(result).some((key) => key !== "requestId" && key !== "sequence") ||
          result.requestId !== requestId ||
          !FIXTURE_REQUEST_ID.test(result.requestId) ||
          !Number.isSafeInteger(result.sequence) ||
          result.sequence < 0
        ) {
          return;
        }
        finish({ sequence: result.sequence });
      };
      documentRef.addEventListener(FIXTURE_FLASH_RESULT_EVENT, onResult);
      session.fixtureFlashCancel = cancel;
      timeout = root.setTimeout(cancel, FIXTURE_FLASH_TIMEOUT_MS);
      documentRef.dispatchEvent(
        new root.CustomEvent(FIXTURE_FLASH_REQUEST_EVENT, {
          detail: JSON.stringify({ requestId, id, rgb }),
        }),
      );
    });
  }

  function installLatencyChannel(peer) {
    if (session.config.fixtureLatency !== true || typeof peer.createDataChannel !== "function") {
      return;
    }
    const channel = peer.createDataChannel("wscrpt-latency", { ordered: true });
    session.dataChannel = channel;
    channel.onmessage = async (event) => {
      let message;
      try {
        message = JSON.parse(String(event.data));
      } catch {
        return;
      }
      if (
        message?.type !== "latency-probe" ||
        !FIXTURE_ID.test(message.id ?? "") ||
        !Array.isArray(message.rgb) ||
        message.rgb.length !== 3 ||
        message.rgb.some(
          (component) => !Number.isInteger(component) || component < 0 || component > 255,
        ) ||
        Object.keys(message).some((key) => key !== "type" && key !== "id" && key !== "rgb")
      ) {
        return;
      }
      const activeSession = session;
      const result = await requestFixtureFlash(message.id, message.rgb);
      if (session === activeSession && session?.peer === peer && result && channel.readyState === "open") {
        channel.send(
          JSON.stringify({
            type: "latency-flash-armed",
            id: message.id,
            sequence: result.sequence,
          }),
        );
      }
    };
  }

  function compactSenderStats(report, sampledAt) {
    const values = [];
    report?.forEach?.((value) => values.push(value));
    const outbound = values.find(
      (entry) =>
        entry.type === "outbound-rtp" &&
        !entry.isRemote &&
        (entry.kind === "video" || entry.mediaType === "video"),
    );
    const codec = values.find(
      (entry) => entry.type === "codec" && entry.id === outbound?.codecId,
    );
    const previous = session.previousStats;
    const elapsed = previous ? Math.max(0.001, (sampledAt - previous.sampledAt) / 1_000) : null;
    const bytes = outbound?.bytesSent ?? 0;
    const stats = {
      sampledAt,
      profile: session.profile,
      sourceWidth: session.canvas.width,
      sourceHeight: session.canvas.height,
      frameWidth: outbound?.frameWidth ?? 0,
      frameHeight: outbound?.frameHeight ?? 0,
      framesPerSecond: outbound?.framesPerSecond ?? null,
      framesEncoded: outbound?.framesEncoded ?? 0,
      framesSent: outbound?.framesSent ?? 0,
      bytesSent: bytes,
      bitrateBps:
        previous && elapsed
          ? Math.round((Math.max(0, bytes - previous.bytesSent) * 8) / elapsed)
          : null,
      codec: codec?.mimeType ?? null,
    };
    session.previousStats = stats;
    return stats;
  }

  function startStatsPump(peer) {
    if (session.statsTimer !== null) root.clearInterval(session.statsTimer);
    session.statsTimer = root.setInterval(async () => {
      if (!session || session.peer !== peer || session.statsBusy) return;
      session.statsBusy = true;
      try {
        const report = await peer.getStats(session.sender?.track);
        if (session?.peer !== peer) return;
        emit("stats", { stats: compactSenderStats(report, root.performance?.now?.() ?? Date.now()) });
      } catch (error) {
        emitError(error, "sender-stats-failed");
      } finally {
        if (session) session.statsBusy = false;
      }
    }, 1_000);
  }

  async function createOffer() {
    closePeer();
    if (!session) return;
    const peer = new root.RTCPeerConnection({ iceServers: [] });
    session.peer = peer;
    peer.onicecandidate = (event) => {
      if (session?.peer !== peer || !event.candidate) return;
      try {
        emit("ice", { candidate: compactCandidate(event.candidate) });
      } catch (error) {
        emitError(error, "sender-ice-failed");
      }
    };
    peer.onconnectionstatechange = () => {
      if (session?.peer !== peer) return;
      emit("state", { state: peer.connectionState });
    };

    let capture = createCapture(session.profile);
    session.capture = capture;
    const transceiver = peer.addTransceiver(capture.track, {
      direction: "sendonly",
      streams: [capture.stream],
    });
    session.sender = transceiver.sender;
    preferH264(transceiver);
    const configured = await configureSender(session.sender, session.profile, capture);
    if (!configured && capture.direct) {
      const mirror = createCapture(session.profile, true);
      await session.sender.replaceTrack(mirror.track);
      stopCapture(capture);
      capture = mirror;
      session.capture = mirror;
      await configureSender(session.sender, session.profile, mirror);
    }
    installLatencyChannel(peer);
    const offer = await peer.createOffer();
    if (session?.peer !== peer) return;
    await peer.setLocalDescription(offer);
    startStatsPump(peer);
    emit("offer", { description: peer.localDescription ?? offer });
  }

  async function acceptAnswer(description) {
    if (!session?.peer || description?.type !== "answer") return;
    await session.peer.setRemoteDescription(description);
    session.remoteDescriptionReady = true;
    const pending = session.pendingCandidates;
    session.pendingCandidates = [];
    for (const candidate of pending) await session.peer?.addIceCandidate(candidate);
  }

  async function acceptCandidate(candidate) {
    if (!session?.peer || candidate === undefined) return;
    if (!session.remoteDescriptionReady) {
      if (session.pendingCandidates.length >= MAX_PENDING_CANDIDATES) {
        throw new Error("too many pending ICE candidates");
      }
      session.pendingCandidates.push(candidate);
      return;
    }
    await session.peer.addIceCandidate(candidate);
  }

  async function replaceProfile(profile) {
    requireProfile(profile);
    if (!session) return;
    session.profile = profile;
    if (!session.sender) return;
    let replacement;
    try {
      replacement = createCapture(profile);
      const configured = await configureSender(session.sender, profile, replacement);
      if (!configured && replacement.direct) {
        stopCapture(replacement);
        replacement = createCapture(profile, true);
        await configureSender(session.sender, profile, replacement);
      }
      await session.sender.replaceTrack(replacement.track);
    } catch (error) {
      stopCapture(replacement);
      throw error;
    }
    const previous = session.capture;
    session.capture = replacement;
    stopCapture(previous);
    emit("state", {
      state: "profile-applied",
      reason: profile,
    });
  }

  function validateInbound(input) {
    const encoded = typeof input === "string" ? input : JSON.stringify(input);
    if (encodedLength(encoded) > MAX_MESSAGE_BYTES) throw new Error("signal exceeds 64 KiB");
    const message = typeof input === "string" ? JSON.parse(input) : input;
    if (
      !message ||
      message.protocolVersion !== VERSION ||
      message.sessionId !== session?.config.sessionId ||
      message.nonce !== session?.config.nonce ||
      !Number.isSafeInteger(message.generation)
    ) {
      return null;
    }
    if (message.generation < session.generation) return null;
    return message;
  }

  async function receive(input) {
    if (!session) return false;
    let message;
    try {
      message = validateInbound(input);
      if (!message) return false;
      if (message.generation > session.generation) {
        closePeer();
        session.generation = message.generation;
      }
      switch (message.type) {
        case "join":
          if (message.profile) session.profile = requireProfile(message.profile);
          await createOffer();
          break;
        case "answer":
          await acceptAnswer(message.description);
          break;
        case "ice":
          await acceptCandidate(message.candidate);
          break;
        case "profile":
          await replaceProfile(message.profile);
          break;
        case "leave":
          closePeer();
          break;
        case "state":
          if (message.state === "restarting") closePeer();
          break;
        default:
          return false;
      }
      return true;
    } catch (error) {
      closePeer();
      emitError(error);
      return false;
    }
  }

  async function start(config) {
    validateBootstrap(config);
    await stop({ reason: "sender-reconfigured", quiet: true });
    const canvas = resolveExactCanvas(config.canvasSelector);
    session = {
      config: { ...config },
      generation: config.generation,
      profile: config.profile ?? "mini",
      canvas,
      peer: null,
      sender: null,
      capture: null,
      dataChannel: null,
      pendingCandidates: [],
      remoteDescriptionReady: false,
      statsTimer: null,
      statsBusy: false,
      previousStats: null,
      fixtureFlashCancel: null,
    };
    emit("state", {
      state: "ready",
    });
    return snapshot();
  }

  async function stop({ reason = "sender-stop", quiet = false } = {}) {
    if (!session) return;
    if (!quiet) {
      try {
        emit("state", { state: "closed", reason });
      } catch {
        // Teardown cannot depend on signaling.
      }
    }
    closePeer();
    session = null;
  }

  function snapshot() {
    if (!session) return { version: VERSION, state: "idle" };
    return {
      version: VERSION,
      state: session.peer ? session.peer.connectionState : "ready",
      sessionId: session.config.sessionId,
      generation: session.generation,
      profile: session.profile,
      sourceWidth: session.canvas.width,
      sourceHeight: session.canvas.height,
    };
  }

  const api = Object.freeze({
    version: VERSION,
    bindingName: BINDING_NAME,
    profiles: PROFILES,
    start,
    receive,
    stop,
    snapshot,
  });
  root.__wscrptPreviewSender = api;
  root.__wscrptPreviewReceive = receive;
})();
