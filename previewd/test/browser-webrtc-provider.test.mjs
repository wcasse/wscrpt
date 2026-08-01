import assert from "node:assert/strict";
import test from "node:test";

import {
  WebRtcPreviewProvider,
  assertLoopbackWebSocketUrl,
  resolveSignalingUrl,
  validateReceiverSession,
} from "../public/webrtc-provider.mjs";
import { validateSignalMessage } from "../src/protocol.mjs";

const tick = () => new Promise((resolve) => setImmediate(resolve));

class FakeWebSocket {
  static instances = [];

  constructor(url) {
    this.url = url;
    this.readyState = 0;
    this.listeners = new Map();
    this.sent = [];
    this.bufferedAmount = 0;
    FakeWebSocket.instances.push(this);
  }

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type, listener) {
    this.listeners.set(
      type,
      (this.listeners.get(type) ?? []).filter((candidate) => candidate !== listener),
    );
  }

  emit(type, value = {}) {
    if (type === "open") this.readyState = 1;
    for (const listener of this.listeners.get(type) ?? []) listener(value);
  }

  send(value) {
    this.sent.push(JSON.parse(value));
  }

  close() {
    this.readyState = 3;
  }
}

class FakeMediaStream {
  constructor() {
    this.tracks = [];
  }

  getTracks() {
    return [...this.tracks];
  }

  addTrack(track) {
    this.tracks.push(track);
  }

  removeTrack(track) {
    this.tracks = this.tracks.filter((candidate) => candidate !== track);
  }
}

class FakePeerConnection {
  static instances = [];

  constructor(configuration) {
    this.configuration = configuration;
    this.connectionState = "new";
    this.transceiver = { setCodecPreferences() {} };
    this.closed = false;
    FakePeerConnection.instances.push(this);
  }

  async setRemoteDescription(description) {
    this.remoteDescription = description;
  }

  async createAnswer() {
    return { type: "answer", sdp: "receiver-answer" };
  }

  async setLocalDescription(description) {
    this.localDescription = description;
  }

  getTransceivers() {
    return [this.transceiver];
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

class DelayedPeerConnection extends FakePeerConnection {
  static releaseFirst = null;

  async setRemoteDescription(description) {
    this.remoteDescription = description;
    if (description.sdp === "offer-one") {
      await new Promise((resolvePromise) => {
        DelayedPeerConnection.releaseFirst = resolvePromise;
      });
    }
  }
}

function envelope(type, generation = 3, payload = {}) {
  return {
    protocolVersion: 1,
    sessionId: "session-1",
    generation,
    nonce: "nonce_1234567890abcdef",
    type,
    ...payload,
  };
}

function session() {
  return {
    protocolVersion: 1,
    sessionId: "session-1",
    generation: 3,
    nonce: "nonce_1234567890abcdef",
    token: "token_1234567890abcdefghijklmnop",
    signaling: { url: "ws://127.0.0.1:7331/signal" },
  };
}

test("signaling URL accepts only an explicit numeric loopback /signal endpoint", () => {
  assert.equal(
    assertLoopbackWebSocketUrl("ws://127.42.0.1:7331/signal").href,
    "ws://127.42.0.1:7331/signal",
  );
  assert.equal(
    assertLoopbackWebSocketUrl("ws://[::1]:7331/signal").hostname,
    "[::1]",
  );
  for (const url of [
    "ws://192.168.1.2:7331/signal",
    "ws://localhost:7331/signal",
    "ws://127.0.0.1:7331/other",
    "ws://127.0.0.1:7331/signal?token=secret",
    "https://127.0.0.1:7331/signal",
  ]) {
    assert.throws(() => assertLoopbackWebSocketUrl(url));
  }
});

test("signaling URL derives from the clean loopback player origin", () => {
  const url = resolveSignalingUrl(
    {},
    { protocol: "http:", hostname: "127.0.0.1", port: "8444" },
  );
  assert.equal(url.href, "ws://127.0.0.1:8444/signal");
  assert.throws(() =>
    resolveSignalingUrl({}, { protocol: "http:", hostname: "remotehost.local", port: "8444" }),
  );
  assert.throws(() =>
    resolveSignalingUrl(
      { signaling: { url: "ws://127.0.0.1:7331/signal" } },
      { protocol: "http:", hostname: "127.0.0.1", port: "8444" },
    ),
  );
});

test("receiver descriptor requires the one-use token and monotonic generation fields", () => {
  assert.equal(validateReceiverSession(session()).sessionId, "session-1");
  assert.throws(() => validateReceiverSession({ ...session(), token: "" }), /token/);
  assert.throws(() => validateReceiverSession({ ...session(), generation: -1 }), /generation/);
  assert.throws(() => validateReceiverSession({ ...session(), protocolVersion: 2 }), /protocol/);
});

test("receiver performs tokened join, answers sender offer, and rejoins at generation+1", async () => {
  FakeWebSocket.instances = [];
  FakePeerConnection.instances = [];
  const provider = new WebRtcPreviewProvider({
    WebSocketClass: FakeWebSocket,
    RTCPeerConnectionClass: FakePeerConnection,
    MediaStreamClass: FakeMediaStream,
    joinTimeoutMs: 1_000,
  });
  const connecting = provider.connect({ session: session(), profile: "mini" });
  const socket = FakeWebSocket.instances[0];
  socket.emit("open");
  assert.deepEqual(socket.sent[0], {
    ...envelope("join"),
    role: "receiver",
    token: "token_1234567890abcdefghijklmnop",
    profile: "mini",
  });

  socket.emit("message", {
    data: JSON.stringify(envelope("joined", 3, { profile: "mini" })),
  });
  const connection = await connecting;
  assert.equal(FakePeerConnection.instances.length, 0, "sender remains the offerer");

  socket.emit("message", {
    data: JSON.stringify(
      envelope("offer", 3, { description: { type: "offer", sdp: "sender-offer" } }),
    ),
  });
  await tick();
  await tick();
  const peer = FakePeerConnection.instances[0];
  assert.deepEqual(peer.configuration, { iceServers: [] });
  assert.equal(peer.remoteDescription.sdp, "sender-offer");
  assert.equal(socket.sent.at(-1).type, "answer");
  assert.equal(socket.sent.at(-1).description.sdp, "receiver-answer");

  let stopped = false;
  const track = { id: "remote-video", kind: "video", stop: () => (stopped = true) };
  peer.ontrack({ track });
  assert.deepEqual(connection.stream.getTracks(), [track]);

  connection.reportStats({
    presentedFps: 24,
    frameAgeMs: 12,
    packetLossRatio: 0,
    rttMs: 20,
  });
  assert.deepEqual(socket.sent.at(-1).stats, {
    presentedFps: 24,
    frameAgeMs: 12,
    packetLossRatio: 0,
    rttMs: 20,
  });

  await connection.restart("stale-frame-age");
  assert.equal(peer.closed, true);
  assert.equal(stopped, true);
  assert.equal(socket.sent.at(-2).type, "state");
  assert.equal(socket.sent.at(-2).generation, 4);
  assert.deepEqual(socket.sent.at(-1), {
    ...envelope("join", 4),
    role: "receiver",
    profile: "mini",
  });
  assert.equal(Object.hasOwn(socket.sent.at(-1), "token"), false);

  for (const signal of socket.sent) {
    assert.doesNotThrow(() => validateSignalMessage(signal, { allowTokenlessJoin: true }));
  }

  connection.close();
  assert.equal(socket.readyState, 3);
});

test("receiver ignores signaling from a stale generation", async () => {
  FakeWebSocket.instances = [];
  FakePeerConnection.instances = [];
  const provider = new WebRtcPreviewProvider({
    WebSocketClass: FakeWebSocket,
    RTCPeerConnectionClass: FakePeerConnection,
    MediaStreamClass: FakeMediaStream,
  });
  const connecting = provider.connect({ session: session() });
  const socket = FakeWebSocket.instances[0];
  socket.emit("open");
  socket.emit("message", {
    data: JSON.stringify(envelope("joined", 3, { profile: "mini" })),
  });
  const connection = await connecting;
  socket.emit("message", {
    data: JSON.stringify(
      envelope("offer", 2, { description: { type: "offer", sdp: "stale" } }),
    ),
  });
  await tick();
  assert.equal(FakePeerConnection.instances.length, 0);
  connection.close();
});

test("offer-before-joined ordering retains playing state for late surface listeners", async () => {
  FakeWebSocket.instances = [];
  FakePeerConnection.instances = [];
  const provider = new WebRtcPreviewProvider({
    WebSocketClass: FakeWebSocket,
    RTCPeerConnectionClass: FakePeerConnection,
    MediaStreamClass: FakeMediaStream,
  });
  const connecting = provider.connect({ session: session() });
  const socket = FakeWebSocket.instances[0];
  socket.emit("open");
  socket.emit("message", {
    data: JSON.stringify(
      envelope("offer", 3, { description: { type: "offer", sdp: "early-offer" } }),
    ),
  });
  await tick();
  await tick();
  const peer = FakePeerConnection.instances[0];
  peer.ontrack({ track: { id: "early-track", kind: "video", stop() {} } });
  peer.connectionState = "connected";
  peer.onconnectionstatechange();
  socket.emit("message", {
    data: JSON.stringify(envelope("joined", 3, { profile: "mini" })),
  });
  const connection = await connecting;
  const states = [];
  connection.onState((state) => states.push(state));
  assert.equal(states[0].state, "playing");
  socket.emit("message", {
    data: JSON.stringify(envelope("state", 3, { state: "profile-applied" })),
  });
  socket.emit("message", {
    data: JSON.stringify(envelope("state", 3, { state: "connecting" })),
  });
  await tick();
  assert.equal(states.at(-1).state, "playing");
  connection.close();
});

test("receiver serializes offer, ICE, replacement offer, and tokenless reload generations", async () => {
  FakeWebSocket.instances = [];
  FakePeerConnection.instances = [];
  DelayedPeerConnection.releaseFirst = null;
  const provider = new WebRtcPreviewProvider({
    WebSocketClass: FakeWebSocket,
    RTCPeerConnectionClass: DelayedPeerConnection,
    MediaStreamClass: FakeMediaStream,
  });
  const connecting = provider.connect({ session: session() });
  const socket = FakeWebSocket.instances[0];
  socket.emit("open");
  socket.emit("message", {
    data: JSON.stringify(envelope("joined", 3, { profile: "mini" })),
  });
  const connection = await connecting;

  socket.emit("message", {
    data: JSON.stringify(
      envelope("offer", 3, { description: { type: "offer", sdp: "offer-one" } }),
    ),
  });
  socket.emit("message", {
    data: JSON.stringify(envelope("ice", 3, { candidate: { candidate: "candidate-one" } })),
  });
  socket.emit("message", {
    data: JSON.stringify(
      envelope("offer", 3, { description: { type: "offer", sdp: "offer-two" } }),
    ),
  });
  await tick();
  assert.equal(FakePeerConnection.instances.length, 1, "later signals wait for offer one");
  DelayedPeerConnection.releaseFirst();
  for (let index = 0; index < 6; index += 1) await tick();

  const [first, second] = FakePeerConnection.instances;
  assert.equal(first.closed, true);
  assert.deepEqual(first.candidates, [{ candidate: "candidate-one" }]);
  assert.equal(second.remoteDescription.sdp, "offer-two");
  assert.equal(
    socket.sent.filter((message) => message.type === "answer").length,
    2,
  );

  await connection.restart("source-navigation");
  socket.emit("message", {
    data: JSON.stringify(envelope("joined", 4, { profile: "mini" })),
  });
  socket.emit("message", {
    data: JSON.stringify(
      envelope("offer", 4, { description: { type: "offer", sdp: "offer-reload" } }),
    ),
  });
  socket.emit("message", {
    data: JSON.stringify(envelope("ice", 4, { candidate: { candidate: "candidate-reload" } })),
  });
  for (let index = 0; index < 6; index += 1) await tick();

  const reloaded = FakePeerConnection.instances.at(-1);
  assert.equal(second.closed, true);
  assert.equal(reloaded.remoteDescription.sdp, "offer-reload");
  assert.deepEqual(reloaded.candidates, [{ candidate: "candidate-reload" }]);
  assert.equal(connection.getGeneration(), 4);
  connection.close();
});

test("receiver closes instead of accumulating inbound or outbound signaling", async () => {
  FakeWebSocket.instances = [];
  FakePeerConnection.instances = [];
  DelayedPeerConnection.releaseFirst = null;
  const inboundProvider = new WebRtcPreviewProvider({
    WebSocketClass: FakeWebSocket,
    RTCPeerConnectionClass: DelayedPeerConnection,
    MediaStreamClass: FakeMediaStream,
    maxInboundSignals: 2,
  });
  const inboundConnecting = inboundProvider.connect({ session: session() });
  const inboundSocket = FakeWebSocket.instances[0];
  inboundSocket.emit("open");
  inboundSocket.emit("message", {
    data: JSON.stringify(envelope("joined", 3, { profile: "mini" })),
  });
  const inbound = await inboundConnecting;
  const inboundStates = [];
  inbound.onState((state) => inboundStates.push(state));
  inboundSocket.emit("message", {
    data: JSON.stringify(
      envelope("offer", 3, { description: { type: "offer", sdp: "offer-one" } }),
    ),
  });
  await tick();
  inboundSocket.emit("message", {
    data: JSON.stringify(envelope("ice", 3, { candidate: { candidate: "queued" } })),
  });
  inboundSocket.emit("message", {
    data: JSON.stringify(envelope("ice", 3, { candidate: { candidate: "overflow" } })),
  });
  assert.equal(inboundSocket.readyState, 3);
  assert.match(inboundStates.at(-1).message, /inbound queue limit/);
  DelayedPeerConnection.releaseFirst();
  await tick();

  FakeWebSocket.instances = [];
  FakePeerConnection.instances = [];
  const outboundProvider = new WebRtcPreviewProvider({
    WebSocketClass: FakeWebSocket,
    RTCPeerConnectionClass: FakePeerConnection,
    MediaStreamClass: FakeMediaStream,
    maxBufferedSignalBytes: 64 * 1024,
  });
  const outboundConnecting = outboundProvider.connect({ session: session() });
  const outboundSocket = FakeWebSocket.instances[0];
  outboundSocket.emit("open");
  outboundSocket.emit("message", {
    data: JSON.stringify(envelope("joined", 3, { profile: "mini" })),
  });
  const outbound = await outboundConnecting;
  const outboundStates = [];
  outbound.onState((state) => outboundStates.push(state));
  outboundSocket.bufferedAmount = 64 * 1024;
  assert.throws(() => outbound.reportStats({ presentedFps: 24 }), /buffer limit/);
  assert.equal(outboundSocket.readyState, 3);
  assert.match(outboundStates.at(-1).message, /outbound buffer limit/);
});

test("receiver rejects non-text and oversized signals before queueing them", async () => {
  for (const raw of [new ArrayBuffer(8), "x".repeat(64 * 1024 + 1)]) {
    FakeWebSocket.instances = [];
    FakePeerConnection.instances = [];
    const provider = new WebRtcPreviewProvider({
      WebSocketClass: FakeWebSocket,
      RTCPeerConnectionClass: FakePeerConnection,
      MediaStreamClass: FakeMediaStream,
    });
    const connecting = provider.connect({ session: session() });
    const socket = FakeWebSocket.instances[0];
    socket.emit("open");
    socket.emit("message", {
      data: JSON.stringify(envelope("joined", 3, { profile: "mini" })),
    });
    const connection = await connecting;
    const states = [];
    connection.onState((state) => states.push(state));
    socket.emit("message", { data: raw });
    assert.equal(socket.readyState, 3);
    assert.equal(states.at(-1).state, "error");
    assert.match(states.at(-1).message, /text messages|64 KiB/);
  }
});

test("signaling close deterministically stops the peer and remote tracks", async () => {
  FakeWebSocket.instances = [];
  FakePeerConnection.instances = [];
  const provider = new WebRtcPreviewProvider({
    WebSocketClass: FakeWebSocket,
    RTCPeerConnectionClass: FakePeerConnection,
    MediaStreamClass: FakeMediaStream,
  });
  const connecting = provider.connect({ session: session() });
  const socket = FakeWebSocket.instances[0];
  socket.emit("open");
  socket.emit("message", {
    data: JSON.stringify(envelope("joined", 3, { profile: "mini" })),
  });
  const connection = await connecting;
  const states = [];
  connection.onState((state) => states.push(state));
  socket.emit("message", {
    data: JSON.stringify(
      envelope("offer", 3, { description: { type: "offer", sdp: "sender-offer" } }),
    ),
  });
  await tick();
  await tick();
  const peer = FakePeerConnection.instances[0];
  let stopped = false;
  const track = { id: "remote-video", kind: "video", stop: () => (stopped = true) };
  peer.ontrack({ track });
  assert.deepEqual(connection.stream.getTracks(), [track]);

  socket.emit("close");
  assert.equal(peer.closed, true);
  assert.equal(stopped, true);
  assert.deepEqual(connection.stream.getTracks(), []);
  assert.match(states.at(-1).message, /connection closed/);
});
