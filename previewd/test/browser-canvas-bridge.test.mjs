import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

import { validateSignalMessage } from "../src/protocol.mjs";

class FakeCanvas {
  constructor(width = 1280, height = 720) {
    this.width = width;
    this.height = height;
    this.captureRates = [];
    this.tracks = [];
  }

  captureStream(fps) {
    this.captureRates.push(fps);
    const track = {
      id: `track-${this.tracks.length + 1}`,
      kind: "video",
      stopped: false,
      stop() {
        this.stopped = true;
      },
    };
    this.tracks.push(track);
    return {
      getVideoTracks: () => [track],
      getTracks: () => [track],
    };
  }

  getContext() {
    return { drawImage() {} };
  }
}

class FakeCustomEvent {
  constructor(type, options = {}) {
    this.type = type;
    this.detail = options.detail;
  }
}

class FakeDocument {
  constructor(sourceCanvas) {
    this.sourceCanvas = sourceCanvas;
    this.listeners = new Map();
  }

  querySelectorAll(selector) {
    return selector === "canvas#game" ? [this.sourceCanvas] : [];
  }

  createElement() {
    return new FakeCanvas(960, 540);
  }

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) ?? new Set();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type, listener) {
    this.listeners.get(type)?.delete(listener);
  }

  dispatchEvent(event) {
    for (const listener of this.listeners.get(event.type) ?? []) listener(event);
    return true;
  }

  listenerCount(type) {
    return this.listeners.get(type)?.size ?? 0;
  }
}

class FakeDataChannel {
  constructor(label, options) {
    this.label = label;
    this.options = options;
    this.readyState = "open";
    this.sent = [];
  }

  send(value) {
    this.sent.push(JSON.parse(value));
  }

  close() {
    this.readyState = "closed";
  }
}

class FakePeerConnection {
  static instances = [];

  constructor(configuration) {
    this.configuration = configuration;
    this.connectionState = "new";
    this.sender = {
      track: null,
      replacements: [],
      getParameters: () => ({ encodings: [{}] }),
      setParameters: async (parameters) => {
        this.sender.parameters = parameters;
      },
      replaceTrack: async (track) => {
        this.sender.track = track;
        this.sender.replacements.push(track);
      },
    };
    FakePeerConnection.instances.push(this);
  }

  addTransceiver(track) {
    this.sender.track = track;
    return { sender: this.sender, setCodecPreferences() {} };
  }

  createDataChannel(label, options) {
    this.dataChannel = new FakeDataChannel(label, options);
    return this.dataChannel;
  }

  async createOffer() {
    return { type: "offer", sdp: `offer-${FakePeerConnection.instances.length}` };
  }

  async setLocalDescription(description) {
    this.localDescription = description;
  }

  async setRemoteDescription(description) {
    this.remoteDescription = description;
  }

  async addIceCandidate(candidate) {
    this.candidates ??= [];
    this.candidates.push(candidate);
  }

  async getStats() {
    return new Map();
  }

  close() {
    this.closed = true;
  }
}

function message(type, generation = 1, payload = {}) {
  return {
    protocolVersion: 1,
    sessionId: "canvas-session",
    generation,
    nonce: "canvas_nonce_1234567890",
    type,
    ...payload,
  };
}

async function loadSender() {
  FakePeerConnection.instances = [];
  const sourceCanvas = new FakeCanvas();
  const document = new FakeDocument(sourceCanvas);
  const outbound = [];
  const context = vm.createContext({
    CustomEvent: FakeCustomEvent,
    TextEncoder,
    HTMLCanvasElement: FakeCanvas,
    RTCPeerConnection: FakePeerConnection,
    RTCRtpSender: { getCapabilities: () => ({ codecs: [] }) },
    crypto: {
      getRandomValues: (bytes) => {
        bytes.fill(0);
        bytes[0] = 1;
        return bytes;
      },
    },
    document,
    performance: { now: () => 100 },
    setTimeout,
    clearTimeout,
    setInterval: () => 1,
    clearInterval() {},
    requestAnimationFrame: () => 1,
    cancelAnimationFrame() {},
    __wscrptPreviewBridge: (encoded) => outbound.push(JSON.parse(encoded)),
  });
  const source = await readFile(
    new URL("../injected/canvas-sender.mjs", import.meta.url),
    "utf8",
  );
  vm.runInContext(source, context, { filename: "canvas-sender.mjs" });
  return { context, document, sourceCanvas, outbound };
}

test("fixed injection installs the exact bridge API and fails closed on canvas identity", async () => {
  const { context, outbound } = await loadSender();
  assert.equal(context.__wscrptPreviewSender.bindingName, "__wscrptPreviewBridge");
  assert.equal(context.__wscrptPreviewReceive, context.__wscrptPreviewSender.receive);
  await assert.rejects(
    context.__wscrptPreviewSender.start({
      protocolVersion: 1,
      sessionId: "canvas-session",
      generation: 1,
      nonce: "canvas_nonce_1234567890",
      canvasSelector: "canvas#missing",
      profile: "mini",
    }),
    /matched 0 elements/,
  );
  assert.equal(outbound.length, 0);
});

test("sender creates the offer and replaces capture tracks for 24/12/30 profiles", async () => {
  const { context, sourceCanvas, outbound } = await loadSender();
  const sender = context.__wscrptPreviewSender;
  await sender.start({
    protocolVersion: 1,
    sessionId: "canvas-session",
    generation: 1,
    nonce: "canvas_nonce_1234567890",
    canvasSelector: "canvas#game",
    profile: "mini",
    fixtureLatency: false,
  });
  assert.equal(outbound.at(-1).state, "ready");
  assert.equal(sender.snapshot().sourceWidth, 1280);

  assert.equal(await context.__wscrptPreviewReceive(message("join", 1, { profile: "mini" })), true);
  assert.equal(sourceCanvas.captureRates.at(-1), 24);
  assert.equal(FakePeerConnection.instances[0].configuration.iceServers.length, 0);
  assert.equal(outbound.at(-1).type, "offer");
  const firstTrack = sourceCanvas.tracks.at(-1);

  await context.__wscrptPreviewReceive(message("profile", 1, { profile: "fallback" }));
  assert.equal(sourceCanvas.captureRates.at(-1), 12);
  assert.equal(firstTrack.stopped, true);
  assert.equal(outbound.at(-1).state, "profile-applied");

  await context.__wscrptPreviewReceive(
    message("profile", 1, { profile: "expanded-headroom" }),
  );
  assert.equal(sourceCanvas.captureRates.at(-1), 30);
  assert.equal(outbound.at(-1).reason, "expanded-headroom");
  for (const signal of outbound) {
    assert.doesNotThrow(() => validateSignalMessage(signal, { allowTokenlessJoin: true }));
  }
  await sender.stop();
});

test("test-only latency bridge exposes only a correlated fixture flash event", async () => {
  const { context, document } = await loadSender();
  const requests = [];
  document.addEventListener("wscrpt-preview-fixture-flash-v1", (event) => {
    const request = JSON.parse(event.detail);
    requests.push(request);
    assert.deepEqual(Object.keys(request).sort(), ["id", "requestId", "rgb"]);
    assert.equal(JSON.stringify(request).includes("canvas_nonce"), false);
    document.dispatchEvent(
      new FakeCustomEvent("wscrpt-preview-fixture-flash-result-v1", {
        detail: JSON.stringify({ requestId: request.requestId, sequence: 42 }),
      }),
    );
  });

  await context.__wscrptPreviewSender.start({
    protocolVersion: 1,
    sessionId: "canvas-session",
    generation: 1,
    nonce: "canvas_nonce_1234567890",
    canvasSelector: "canvas#game",
    profile: "mini",
    fixtureLatency: true,
  });
  await context.__wscrptPreviewReceive(message("join", 1, { profile: "mini" }));
  const channel = FakePeerConnection.instances[0].dataChannel;
  assert.equal(channel.label, "wscrpt-latency");
  assert.equal(channel.options.ordered, true);

  await channel.onmessage({
    data: JSON.stringify({ type: "latency-probe", id: "probe-1", rgb: [255, 0, 255] }),
  });
  assert.deepEqual(channel.sent, [
    { type: "latency-flash-armed", id: "probe-1", sequence: 42 },
  ]);
  assert.equal(requests.length, 1);
  assert.match(requests[0].requestId, /^[a-f0-9]{32}$/u);
  assert.equal(document.listenerCount("wscrpt-preview-fixture-flash-result-v1"), 0);

  await channel.onmessage({
    data: JSON.stringify({ type: "latency-probe", id: "probe-2", rgb: [999, 0, 0] }),
  });
  assert.equal(requests.length, 1, "invalid RGB never crosses into the fixture page world");
  await context.__wscrptPreviewSender.stop();
});

test("sender drops wrong-session signals and replaces a stale peer on generation advance", async () => {
  const { context, outbound } = await loadSender();
  await context.__wscrptPreviewSender.start({
    protocolVersion: 1,
    sessionId: "canvas-session",
    generation: 1,
    nonce: "canvas_nonce_1234567890",
    canvasSelector: "canvas#game",
    profile: "mini",
  });
  await context.__wscrptPreviewReceive(message("join"));
  const firstPeer = FakePeerConnection.instances[0];
  const count = outbound.length;
  assert.equal(
    await context.__wscrptPreviewReceive({ ...message("profile"), nonce: "wrong" }),
    false,
  );
  assert.equal(outbound.length, count);

  await context.__wscrptPreviewReceive(message("join", 2, { profile: "mini" }));
  assert.equal(firstPeer.closed, true);
  assert.equal(FakePeerConnection.instances.length, 2);
  assert.equal(outbound.at(-1).generation, 2);
  assert.equal(outbound.at(-1).type, "offer");
});
