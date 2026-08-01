import test from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PreviewDaemon, SignalingSession } from "../src/previewd.mjs";
import { RuntimeStore } from "../src/runtime-store.mjs";

async function fixture(t, Store = RuntimeStore) {
  const parent = await mkdtemp(join(tmpdir(), "wscrpt-previewd-startup-test-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const store = new Store({ root: join(parent, "runtime") });
  const sessionId = "p-startup-transaction";
  const runId = "run-startup-current";
  await store.writePrivateConfig(sessionId, {
    cdpUrl: "http://127.0.0.1:9222",
    urlPattern: "http://127.0.0.1:5173/game",
    fixtureLatency: false,
  });
  await store.writeManifest({
    protocolVersion: 1,
    sessionId,
    runId,
    generation: 0,
    activeGeneration: 0,
    workspace: { canonicalRoot: "/workspace", revision: null },
    tmux: { session: "wscrpt-preview-startup", pane: "%12", owned: true },
    target: {
      id: "clock-target",
      urlHash: "sha256:test",
      canvasSelector: "canvas#game",
      sourceWidth: null,
      sourceHeight: null,
    },
    signaling: null,
    state: "starting",
    heartbeatAt: new Date().toISOString(),
  });
  return { store, sessionId, runId };
}

function resources() {
  const state = {
    cdpClosed: 0,
    signalingClosed: 0,
    webSocketClosed: 0,
    timerCleared: 0,
  };
  const cdp = {
    async attach() { return { width: 960, height: 540 }; },
    async close() { state.cdpClosed += 1; },
  };
  const createSignaling = ({ manifest }) => ({
    manifest,
    accept() {},
    async close() { state.signalingClosed += 1; },
  });
  class FakeWebSocketServer {
    constructor() {
      this.clients = new Set();
      state.webSocketServer = this;
    }

    handleUpgrade() {}

    close(callback) {
      state.webSocketClosed += 1;
      callback?.();
    }
  }
  return { state, cdp, createSignaling, FakeWebSocketServer };
}

test("CDP attach rejection closes the partial client and publishes an error", async (t) => {
  const { store, sessionId, runId } = await fixture(t);
  let closed = 0;
  const failure = Object.assign(new Error("canvas inspection failed"), { code: "canvas_missing" });
  const daemon = new PreviewDaemon({
    store,
    sessionId,
    runId,
    createCdp: () => ({
      async attach() { throw failure; },
      async close() { closed += 1; },
    }),
  });

  await assert.rejects(daemon.start(), (error) => error === failure);
  assert.equal(closed, 1);
  assert.equal(daemon.cdp, null);
  assert.equal(daemon.state, "error");
  const manifest = await store.readManifest(sessionId);
  assert.equal(manifest.state, "error");
  assert.equal(manifest.lastError.code, "canvas_missing");
  assert.equal(manifest.lastError.message, "canvas inspection failed");
});

test("startup rolls back CDP and signaling when WebSocket loading fails", async (t) => {
  const { store, sessionId, runId } = await fixture(t);
  const { state, cdp, createSignaling } = resources();
  const failure = Object.assign(new Error("WebSocket module could not load"), { code: "ws_import_failed" });
  const daemon = new PreviewDaemon({
    store,
    sessionId,
    runId,
    createCdp: () => cdp,
    createSignaling,
    loadWebSocketServer: async () => { throw failure; },
  });

  await assert.rejects(daemon.start(), (error) => error === failure);
  assert.equal(state.cdpClosed, 1);
  assert.equal(state.signalingClosed, 1);
  assert.equal(daemon.cdp, null);
  assert.equal(daemon.signaling, null);
  assert.equal(daemon.server, null);
  assert.equal(daemon.webSocketServer, null);
  assert.equal(daemon.heartbeatTimer, null);
  assert.equal(daemon.state, "error");
  const manifest = await store.readManifest(sessionId);
  assert.equal(manifest.state, "error");
  assert.equal(manifest.daemon, null);
  assert.equal(manifest.signaling, null);
  assert.equal(manifest.lastError.code, "ws_import_failed");
});

class FailReadyStore extends RuntimeStore {
  failReady = false;

  async writeManifest(manifest) {
    if (this.failReady && manifest.state === "ready") {
      this.failReady = false;
      const error = Object.assign(new Error("x".repeat(2_000)), { code: "r".repeat(200) });
      throw error;
    }
    return super.writeManifest(manifest);
  }
}

class ImmediateServer extends EventEmitter {
  constructor(state) {
    super();
    this.state = state;
    this.listening = false;
  }

  listen(_port, _host, callback) {
    this.listening = true;
    queueMicrotask(callback);
  }

  address() {
    return { address: "127.0.0.1", family: "IPv4", port: 7444 };
  }

  close(callback) {
    this.listening = false;
    this.state.serverClosed += 1;
    callback?.();
  }
}

test("startup closes an open listener and clears its timer when ready publication fails", async (t) => {
  const { store, sessionId, runId } = await fixture(t, FailReadyStore);
  store.failReady = true;
  const { state, cdp, createSignaling, FakeWebSocketServer } = resources();
  state.serverClosed = 0;
  const server = new ImmediateServer(state);
  const timer = { unref() {} };
  const daemon = new PreviewDaemon({
    store,
    sessionId,
    runId,
    createCdp: () => cdp,
    createSignaling,
    createHttpServer: () => server,
    loadWebSocketServer: async () => FakeWebSocketServer,
    setIntervalFn: () => timer,
    clearIntervalFn: (value) => {
      assert.equal(value, timer);
      state.timerCleared += 1;
    },
  });

  await assert.rejects(daemon.start(), (error) => error.code === "r".repeat(200));
  assert.equal(server.listening, false);
  assert.equal(state.cdpClosed, 1);
  assert.equal(state.signalingClosed, 1);
  assert.equal(state.webSocketClosed, 1);
  assert.equal(state.timerCleared, 1);
  assert.equal(state.serverClosed, 1);
  assert.equal(daemon.server, null);
  assert.equal(daemon.webSocketServer, null);
  assert.equal(daemon.heartbeatTimer, null);
  const manifest = await store.readManifest(sessionId);
  assert.equal(manifest.state, "error");
  assert.equal(manifest.lastError.code.length, 64);
  assert.equal(manifest.lastError.message.length, 512);
});

class ControlledServer extends EventEmitter {
  constructor(state) {
    super();
    this.state = state;
    this.listening = false;
  }

  listen(_port, _host, callback) {
    this.listening = true;
    this.state.listenEntered.resolve();
    this.state.finishListen = callback;
  }

  address() {
    return { address: "127.0.0.1", family: "IPv4", port: 7444 };
  }

  close(callback) {
    this.listening = false;
    this.state.serverClosed += 1;
    callback?.();
  }
}

test("a superseded daemon cannot publish ready or revoke replacement-run tokens", async (t) => {
  const { store, sessionId, runId } = await fixture(t);
  const { state, cdp, createSignaling, FakeWebSocketServer } = resources();
  state.listenEntered = { ...(() => {
    let resolve;
    const promise = new Promise((resolvePromise) => { resolve = resolvePromise; });
    return { promise, resolve };
  })() };
  state.serverClosed = 0;
  const server = new ControlledServer(state);
  const daemon = new PreviewDaemon({
    store,
    sessionId,
    runId,
    createCdp: () => cdp,
    createSignaling,
    createHttpServer: () => server,
    loadWebSocketServer: async () => FakeWebSocketServer,
    setIntervalFn: () => ({ unref() {} }),
    clearIntervalFn: () => { state.timerCleared += 1; },
  });

  const starting = daemon.start();
  await state.listenEntered.promise;
  const replacementRunId = "run-startup-replacement";
  const current = await store.readManifest(sessionId);
  await store.writeManifest({
    ...current,
    runId: replacementRunId,
    state: "starting",
    daemon: null,
    signaling: null,
  });
  const replacementToken = await store.issueToken({ sessionId, generation: 1 });
  state.finishListen();

  await assert.rejects(starting, (error) => error.code === "session_superseded");
  const replacement = await store.readManifest(sessionId);
  assert.equal(replacement.runId, replacementRunId);
  assert.equal(replacement.state, "starting");
  assert.equal(replacement.lastError, undefined);
  assert.equal((await store.consumeToken(replacementToken)).generation, 1);
  assert.equal(state.serverClosed, 1);
  assert.equal(state.webSocketClosed, 1);
  assert.equal(state.cdpClosed, 1);
  assert.equal(state.signalingClosed, 1);
  assert.equal(state.timerCleared, 1);
});

test("a superseded signaling callback cannot error the replacement run or revoke its token", async (t) => {
  const { store, sessionId, runId } = await fixture(t);
  const oldManifest = {
    ...(await store.readManifest(sessionId)),
    state: "ready",
    signaling: { host: "127.0.0.1", port: 7331, path: "/signal" },
  };
  await store.writeManifest(oldManifest);
  let stoppedSender = 0;
  const errors = [];
  const signaling = new SignalingSession({
    store,
    manifest: oldManifest,
    cdp: {
      async stopSender() { stoppedSender += 1; },
    },
    onError: (error) => errors.push(error),
  });
  const replacementRunId = `${runId}-replacement`;
  await store.writeManifest({
    ...oldManifest,
    runId: replacementRunId,
    state: "starting",
    signaling: null,
  });
  const replacementToken = await store.issueToken({ sessionId, generation: 1 });

  await signaling.handleSourceLost("old-cdp-disconnected");

  const replacement = await store.readManifest(sessionId);
  assert.equal(replacement.runId, replacementRunId);
  assert.equal(replacement.state, "starting");
  assert.equal(replacement.lastError, undefined);
  assert.equal((await store.consumeToken(replacementToken)).generation, 1);
  assert.equal(stoppedSender, 1);
  assert.equal(errors.some((error) => error.code === "session_superseded"), true);
});
