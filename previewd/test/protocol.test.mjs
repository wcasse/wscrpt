import test from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  MAX_ICE_CANDIDATES,
  FixedWindowRateLimiter,
  ProtocolError,
  assertLoopbackHost,
  assertLoopbackUrl,
  serializeSignalMessage,
  validateSignalMessage,
} from "../src/protocol.mjs";
import { SignalingSession, createHttpHandler, validateWebSocketUpgradeRequest } from "../src/previewd.mjs";
import { RuntimeStore } from "../src/runtime-store.mjs";

const nonce = "abcdefghijklmnop";
const token = "abcdefghijklmnopqrstuvwxyzABCDEFGH";

function message(type, payload = {}) {
  return {
    protocolVersion: 1,
    sessionId: "p-0123456789abcdef",
    generation: 1,
    nonce,
    type,
    ...payload,
  };
}

function errorCode(operation, code) {
  assert.throws(operation, (error) => error instanceof ProtocolError && error.code === code);
}

class FakeSocket extends EventEmitter {
  constructor() {
    super();
    this.readyState = 1;
    this.bufferedAmount = 0;
    this.sent = [];
    this.closes = [];
  }

  send(encoded) {
    this.sent.push(JSON.parse(encoded));
  }

  close(code, reason) {
    if (this.readyState !== 1) return;
    this.readyState = 3;
    this.closes.push({ code, reason });
    this.emit("close");
  }

  signal(value) {
    this.emit("message", Buffer.from(JSON.stringify(value)), false);
  }
}

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => { resolve = resolvePromise; });
  return { promise, resolve };
}

async function drainSignaling(session) {
  for (;;) {
    const current = session.operationQueue;
    await current;
    await new Promise((resolvePromise) => setImmediate(resolvePromise));
    if (current === session.operationQueue) return;
  }
}

async function signalingFixture(t, cdp) {
  const parent = await mkdtemp(join(tmpdir(), "wscrpt-signaling-test-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const store = new RuntimeStore({ root: join(parent, "runtime") });
  const sessionId = "p-signaling-race";
  const manifest = {
    protocolVersion: 1,
    sessionId,
    generation: 0,
    activeGeneration: 0,
    workspace: { canonicalRoot: "/workspace", revision: null },
    tmux: { session: "wscrpt-preview-test", pane: "%1", owned: true },
    target: { id: "target", urlHash: "sha256:test", canvasSelector: "canvas#game" },
    signaling: { host: "127.0.0.1", port: 7331, path: "/signal" },
    state: "ready",
    heartbeatAt: new Date().toISOString(),
  };
  await store.writeManifest(manifest);
  const errors = [];
  const signaling = new SignalingSession({ store, manifest, cdp, onError: (error) => errors.push(error) });
  const reserve = async (generation) => {
    const issued = await store.issueToken({ sessionId, generation });
    await store.updateManifest(sessionId, (current) => ({ ...current, generation }));
    return issued;
  };
  return { store, sessionId, signaling, errors, reserve };
}

function joinMessage(sessionId, issued, profile = "mini") {
  return {
    protocolVersion: 1,
    sessionId,
    generation: issued.generation,
    nonce: issued.nonce,
    type: "join",
    role: "receiver",
    token: issued.token,
    profile,
  };
}

test("validates the tokened initial join and tokenless authenticated rejoin", () => {
  const initial = message("join", { role: "receiver", token, profile: "mini" });
  assert.deepEqual(validateSignalMessage(JSON.stringify(initial)), initial);
  const rejoin = { ...initial, generation: 2 };
  delete rejoin.token;
  errorCode(() => validateSignalMessage(rejoin), "token_required");
  assert.deepEqual(validateSignalMessage(rejoin, { allowTokenlessJoin: true }), rejoin);
});

test("validates canonical SDP, state metadata, and compact stats payloads", () => {
  const offer = message("offer", {
    description: { type: "offer", sdp: "v=0\r\n" },
    reason: "initial",
  });
  assert.equal(validateSignalMessage(offer).description.type, "offer");
  const state = message("state", {
    state: "profile-applied",
    reason: "fallback",
    profile: "fallback",
    sourceWidth: 1280,
    sourceHeight: 720,
    width: 960,
    height: 540,
    fps: 12,
  });
  assert.equal(validateSignalMessage(state).fps, 12);
  const stats = message("stats", {
    stats: { presentedFps: 23.9, frameAgeMs: 82, packetLossRatio: 0.01, rttMs: 34 },
  });
  assert.equal(validateSignalMessage(stats).stats.presentedFps, 23.9);
});

test("rejects cross-session, wrong nonce, stale generation, and unknown fields", () => {
  const state = message("state", { state: "ready" });
  errorCode(() => validateSignalMessage(state, { expectedSessionId: "another" }), "wrong_session");
  errorCode(() => validateSignalMessage(state, { expectedNonce: "qrstuvwxyzABCDEF" }), "wrong_nonce");
  errorCode(() => validateSignalMessage(state, { minimumGeneration: 2 }), "stale_generation");
  errorCode(() => validateSignalMessage({ ...state, secret: "no" }), "unknown_field");
});

test("enforces the 64 KiB bound before parsing or serialization", () => {
  errorCode(() => validateSignalMessage("x".repeat(64 * 1024 + 1)), "message_too_large");
  const large = message("error", { code: "large", message: "x".repeat(64 * 1024) });
  errorCode(() => serializeSignalMessage(large), "message_too_large");
});

test("validates ICE candidates and rejects malformed SDP descriptions", () => {
  const ice = message("ice", {
    candidate: {
      candidate: "candidate:1 1 UDP 1 127.0.0.1 5000 typ host",
      sdpMid: "0",
      sdpMLineIndex: 0,
      usernameFragment: "abcd",
    },
  });
  assert.equal(validateSignalMessage(ice).candidate.sdpMLineIndex, 0);
  errorCode(
    () => validateSignalMessage(message("answer", { description: { type: "offer", sdp: "v=0" } })),
    "invalid_description",
  );
  assert.equal(MAX_ICE_CANDIDATES, 64);
});

test("only numeric loopback hosts and loopback signaling URLs are accepted", () => {
  assert.equal(assertLoopbackHost("127.0.0.1"), "127.0.0.1");
  assert.equal(assertLoopbackHost("127.42.0.9"), "127.42.0.9");
  assert.equal(assertLoopbackHost("[::1]"), "::1");
  errorCode(() => assertLoopbackHost("0.0.0.0"), "non_loopback");
  errorCode(() => assertLoopbackHost("localhost"), "non_loopback");
  assert.equal(assertLoopbackUrl("ws://127.0.0.1:7331/signal").pathname, "/signal");
  errorCode(() => assertLoopbackUrl("ws://192.168.1.20:7331/signal"), "non_loopback");
});

test("fixed-window limiter fails closed above its bound and resets", () => {
  let now = 0;
  const limiter = new FixedWindowRateLimiter({ limit: 2, windowMs: 1000, now: () => now });
  limiter.take();
  limiter.take();
  errorCode(() => limiter.take(), "rate_limited");
  now = 1000;
  limiter.take();
});

test("WebSocket upgrade requires an exact loopback same-origin route", () => {
  const valid = {
    url: "/signal",
    socket: { remoteAddress: "127.0.0.1" },
    headers: { host: "127.0.0.1:49152", origin: "http://127.0.0.1:49152" },
  };
  assert.equal(validateWebSocketUpgradeRequest(valid).path, "/signal");
  assert.throws(() => validateWebSocketUpgradeRequest({ ...valid, headers: { host: valid.headers.host } }));
  assert.throws(() => validateWebSocketUpgradeRequest({
    ...valid,
    headers: { ...valid.headers, origin: "http://127.0.0.1:49153" },
  }));
  assert.throws(() => validateWebSocketUpgradeRequest({ ...valid, url: "/signal?token=secret" }));
  assert.throws(() => validateWebSocketUpgradeRequest({
    ...valid,
    socket: { remoteAddress: "192.168.1.8" },
  }));
});

test("static handler rejects query strings and emits a defensive CSP", async () => {
  let healthState = "ready";
  const handler = createHttpHandler({ sessionId: "p-test", getState: () => healthState });
  const invoke = async (url) => {
    const result = {};
    await handler(
      { method: "GET", url },
      {
        writeHead(status, headers) { result.status = status; result.headers = headers; },
        end(body) { result.body = body; },
      },
    );
    return result;
  };
  const index = await invoke("/");
  assert.equal(index.status, 200);
  for (const directive of ["base-uri 'none'", "form-action 'none'", "frame-ancestors 'none'", "object-src 'none'"]) {
    assert.equal(index.headers["content-security-policy"].includes(directive), true);
  }
  assert.equal((await invoke("/?token=must-not-enter-request-line")).status, 404);
  assert.equal((await invoke("/not-allowlisted.mjs")).status, 404);
  assert.equal(JSON.parse((await invoke("/healthz")).body).state, "ready");
  healthState = "error";
  assert.equal(JSON.parse((await invoke("/healthz")).body).state, "error");
});

test("a queued join cannot consume its token after the socket closes", async (t) => {
  const cdp = {
    starts: 0,
    async startSender() { this.starts += 1; return {}; },
    async receive() {},
    async stopSender() {},
  };
  const fixture = await signalingFixture(t, cdp);
  const issued = await fixture.reserve(1);
  const blocker = deferred();
  fixture.signaling.operationQueue = blocker.promise;
  const socket = new FakeSocket();
  fixture.signaling.accept(socket);
  socket.signal(joinMessage(fixture.sessionId, issued));
  socket.close(1000, "caller closed");
  blocker.resolve();
  await drainSignaling(fixture.signaling);
  assert.equal(cdp.starts, 0);
  assert.equal(fixture.signaling.receiver, null);
  assert.equal((await fixture.store.readManifest(fixture.sessionId)).state, "ready");
  assert.equal((await fixture.store.consumeToken(issued)).generation, 1);
});

test("close during sender start aborts authentication and the next reserved generation attaches", async (t) => {
  const startGate = deferred();
  let blockStart = true;
  const cdp = {
    starts: 0,
    stops: 0,
    received: [],
    async startSender() {
      this.starts += 1;
      if (blockStart) await startGate.promise;
      return { sourceWidth: 1280, sourceHeight: 720 };
    },
    async receive(value) { this.received.push(value); },
    async stopSender() { this.stops += 1; },
  };
  const fixture = await signalingFixture(t, cdp);
  const first = await fixture.reserve(1);
  const firstSocket = new FakeSocket();
  fixture.signaling.accept(firstSocket);
  firstSocket.signal(joinMessage(fixture.sessionId, first));
  while (cdp.starts === 0) await new Promise((resolvePromise) => setImmediate(resolvePromise));
  firstSocket.close(1000, "closed during start");
  blockStart = false;
  startGate.resolve();
  await drainSignaling(fixture.signaling);
  const afterAbort = await fixture.store.readManifest(fixture.sessionId);
  assert.equal(afterAbort.state, "ready");
  assert.equal(afterAbort.generation, 1);
  assert.equal(afterAbort.activeGeneration, 0);
  assert.equal(cdp.stops, 1);

  const second = await fixture.reserve(2);
  const secondSocket = new FakeSocket();
  fixture.signaling.accept(secondSocket);
  secondSocket.signal(joinMessage(fixture.sessionId, second));
  await drainSignaling(fixture.signaling);
  assert.equal((await fixture.store.readManifest(fixture.sessionId)).activeGeneration, 2);
  assert.equal(fixture.signaling.receiver?.socket, secondSocket);
});

test("close during tokenless rejoin consumes that generation so a fresh token advances", async (t) => {
  const rejoinGate = deferred();
  let blockGeneration = null;
  const cdp = {
    async startSender() { return { sourceWidth: 1280, sourceHeight: 720 }; },
    async receive(value) {
      if (value.type === "join" && value.generation === blockGeneration) await rejoinGate.promise;
    },
    async stopSender() {},
  };
  const fixture = await signalingFixture(t, cdp);
  const first = await fixture.reserve(1);
  const firstSocket = new FakeSocket();
  fixture.signaling.accept(firstSocket);
  firstSocket.signal(joinMessage(fixture.sessionId, first));
  await drainSignaling(fixture.signaling);

  blockGeneration = 2;
  firstSocket.signal({
    protocolVersion: 1,
    sessionId: fixture.sessionId,
    generation: 2,
    nonce: first.nonce,
    type: "join",
    role: "receiver",
    profile: "mini",
  });
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  firstSocket.close(1000, "closed during rejoin");
  rejoinGate.resolve();
  await drainSignaling(fixture.signaling);
  const afterRejoin = await fixture.store.readManifest(fixture.sessionId);
  assert.equal(afterRejoin.generation, 2);
  assert.equal(afterRejoin.activeGeneration, 1);

  blockGeneration = null;
  const third = await fixture.reserve(3);
  const nextSocket = new FakeSocket();
  fixture.signaling.accept(nextSocket);
  nextSocket.signal(joinMessage(fixture.sessionId, third));
  await drainSignaling(fixture.signaling);
  assert.equal((await fixture.store.readManifest(fixture.sessionId)).activeGeneration, 3);
});

test("a replacement token reservation wins over an authenticated tokenless rejoin", async (t) => {
  const cdp = {
    async startSender() { return { sourceWidth: 1280, sourceHeight: 720 }; },
    async receive() {},
    async stopSender() {},
  };
  const fixture = await signalingFixture(t, cdp);
  const first = await fixture.reserve(1);
  const firstSocket = new FakeSocket();
  fixture.signaling.accept(firstSocket);
  firstSocket.signal(joinMessage(fixture.sessionId, first));
  await drainSignaling(fixture.signaling);

  const replacement = await fixture.reserve(2);
  firstSocket.signal({
    protocolVersion: 1,
    sessionId: fixture.sessionId,
    generation: 2,
    nonce: first.nonce,
    type: "join",
    role: "receiver",
    profile: "mini",
  });
  await drainSignaling(fixture.signaling);

  const reserved = await fixture.store.readManifest(fixture.sessionId);
  assert.equal(fixture.errors.at(-1)?.code, "generation_reserved");
  assert.equal(firstSocket.sent.at(-1)?.code, "generation_reserved");
  assert.equal(reserved.generation, 2);
  assert.equal(reserved.activeGeneration, 1);

  const replacementSocket = new FakeSocket();
  fixture.signaling.accept(replacementSocket);
  replacementSocket.signal(joinMessage(fixture.sessionId, replacement));
  await drainSignaling(fixture.signaling);
  const connected = await fixture.store.readManifest(fixture.sessionId);
  assert.equal(connected.activeGeneration, 2);
  assert.equal(fixture.signaling.receiver?.socket, replacementSocket);
});

test("target identity loss fails closed in the receiver and private manifest", async (t) => {
  const cdp = {
    stops: 0,
    async startSender() { return { sourceWidth: 1280, sourceHeight: 720 }; },
    async receive() {},
    async stopSender() { this.stops += 1; },
  };
  const fixture = await signalingFixture(t, cdp);
  const issued = await fixture.reserve(1);
  const socket = new FakeSocket();
  fixture.signaling.accept(socket);
  socket.signal(joinMessage(fixture.sessionId, issued));
  await drainSignaling(fixture.signaling);
  await fixture.signaling.handleIdentityLost({ reason: "canvas-identity-lost" });
  await drainSignaling(fixture.signaling);
  const failed = await fixture.store.readManifest(fixture.sessionId);
  assert.equal(failed.state, "error");
  assert.deepEqual(failed.lastError, {
    code: "target_identity_lost",
    message: "Preview target URL or canvas identity changed",
  });
  assert.equal(fixture.signaling.receiver, null);
  assert.equal(socket.sent.at(-1).type, "error");
  assert.equal(socket.sent.at(-1).code, "target_identity_lost");
  assert.equal(socket.closes.at(-1).code, 4002);
  assert.equal(cdp.stops, 1);
});
