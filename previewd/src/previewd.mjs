#!/usr/bin/env node

import { createServer } from "node:http";
import { readFile, realpath, stat } from "node:fs/promises";
import { basename, extname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  MAX_ICE_CANDIDATES,
  MAX_SIGNAL_BYTES,
  PROTOCOL_VERSION,
  FixedWindowRateLimiter,
  ProtocolError,
  assertLoopbackHost,
  assertLoopbackUrl,
  serializeSignalMessage,
  validateSignalMessage,
} from "./protocol.mjs";
import { AdaptationController } from "./adaptation.mjs";
import { CdpSession } from "./cdp-session.mjs";
import { RuntimeStore } from "./runtime-store.mjs";

const PUBLIC_DIRECTORY = fileURLToPath(new URL("../public/", import.meta.url));
const FIXTURE_DIRECTORY = fileURLToPath(new URL("../fixtures/", import.meta.url));
const STATIC_FILES = new Map([
  ["/", [PUBLIC_DIRECTORY, "index.html"]],
  ["/index.html", [PUBLIC_DIRECTORY, "index.html"]],
  ["/preview-surface.mjs", [PUBLIC_DIRECTORY, "preview-surface.mjs"]],
  ["/metrics.mjs", [PUBLIC_DIRECTORY, "metrics.mjs"]],
  ["/webrtc-provider.mjs", [PUBLIC_DIRECTORY, "webrtc-provider.mjs"]],
  ["/jpeg-provider.mjs", [PUBLIC_DIRECTORY, "jpeg-provider.mjs"]],
  ["/fixtures/clock-game.html", [FIXTURE_DIRECTORY, "clock-game.html"]],
  ["/fixtures/clock-game.mjs", [FIXTURE_DIRECTORY, "clock-game.mjs"]],
]);

const CONTENT_TYPES = Object.freeze({
  ".html": "text/html; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
});

const RECEIVER_TYPES = new Set(["join", "answer", "ice", "profile", "stats", "state", "leave"]);
const SENDER_TYPES = new Set(["offer", "ice", "stats", "state", "error", "leave"]);
const MAX_SOCKET_BUFFERED_BYTES = 256 * 1024;

function safeReason(error) {
  if (error instanceof ProtocolError) return { code: error.code, message: error.message };
  return { code: "internal_error", message: "preview signaling failed" };
}

function envelope(sessionId, generation, nonce, type, payload = {}) {
  return { protocolVersion: PROTOCOL_VERSION, sessionId, generation, nonce, type, ...payload };
}

function sendJson(socket, message) {
  if (socket?.readyState !== 1) return false;
  const encoded = serializeSignalMessage(message);
  if (
    Number.isFinite(socket.bufferedAmount) &&
    socket.bufferedAmount + Buffer.byteLength(encoded, "utf8") > MAX_SOCKET_BUFFERED_BYTES
  ) {
    socket.close(1013, "signaling receiver is too slow");
    return false;
  }
  socket.send(encoded);
  return true;
}

function candidateKey(direction) {
  return `${direction}IceCandidates`;
}

export class SignalingSession {
  constructor({ store, manifest, cdp, fixtureLatency = false, now = () => Date.now(), onError = () => {} }) {
    this.store = store;
    this.manifest = manifest;
    this.runId = manifest.runId ?? null;
    this.cdp = cdp;
    this.now = now;
    this.onError = onError;
    this.fixtureLatency = fixtureLatency === true;
    this.receiver = null;
    this.generation = Math.max(0, manifest.activeGeneration ?? 0);
    this.nonce = null;
    this.profile = "mini";
    this.source = {
      width: manifest.target?.sourceWidth ?? null,
      height: manifest.target?.sourceHeight ?? null,
    };
    this.receiverIceCandidates = 0;
    this.senderIceCandidates = 0;
    this.adaptation = new AdaptationController({ primaryProfile: "mini" });
    this.closed = false;
    this.operationQueue = Promise.resolve();
    this.queuedOperations = 0;
    this.senderRateLimiter = new FixedWindowRateLimiter({ now: this.now });
    this.identityLost = false;
  }

  #assertManifestOwnership(manifest, allowedStates) {
    if ((manifest.runId ?? null) !== this.runId) {
      throw new ProtocolError("session_superseded", "preview daemon no longer owns this session run");
    }
    if (allowedStates && !allowedStates.has(manifest.state)) {
      throw new ProtocolError("session_inactive", `preview session is ${manifest.state ?? "invalid"}`);
    }
  }

  accept(socket) {
    const connection = {
      socket,
      authenticated: false,
      generation: 0,
      nonce: null,
      closed: false,
      joined: false,
      joinTimer: null,
      limiter: new FixedWindowRateLimiter({ now: this.now }),
    };
    connection.joinTimer = setTimeout(() => {
      if (!connection.joined && !connection.closed) socket.close(1008, "join timeout");
    }, 10_000);
    connection.joinTimer.unref?.();
    socket.on("message", (raw, isBinary) => {
      if (connection.closed) return;
      if (isBinary || Buffer.byteLength(raw) > MAX_SIGNAL_BYTES) {
        socket.close(1003, "JSON text required");
        return;
      }
      try {
        connection.limiter.take();
      } catch (error) {
        this.#handleReceiverError(connection, error);
        return;
      }
      if (this.queuedOperations >= 8) {
        this.#handleReceiverError(connection, new ProtocolError("queue_full", "signaling work queue is full"));
        return;
      }
      this.queuedOperations += 1;
      this.operationQueue = this.operationQueue
        .then(() => this.#handleReceiverMessage(connection, raw))
        .catch((error) => this.#handleReceiverError(connection, error))
        .finally(() => { this.queuedOperations -= 1; });
    });
    socket.on("close", () => {
      connection.closed = true;
      if (connection.joinTimer) clearTimeout(connection.joinTimer);
      const wasCurrent = this.receiver === connection;
      if (wasCurrent) this.receiver = null;
      this.operationQueue = this.operationQueue
        .then(() => this.#cleanupClosedConnection(connection, wasCurrent))
        .catch(this.onError);
    });
    socket.on("error", this.onError);
    return connection;
  }

  async #handleReceiverMessage(connection, raw) {
    if (this.closed) return;
    if (this.identityLost) throw new ProtocolError("target_identity_lost", "preview target identity was lost");
    this.#requireOpenConnection(connection);
    if (connection.authenticated && this.receiver !== connection) {
      throw new ProtocolError("receiver_replaced", "receiver connection has been replaced");
    }

    if (!connection.authenticated) {
      const join = validateSignalMessage(raw, {
        expectedSessionId: this.manifest.sessionId,
        allowTokenlessJoin: false,
      });
      if (join.type !== "join") throw new ProtocolError("join_required", "first message must be join");
      await this.store.consumeToken({
        token: join.token,
        sessionId: join.sessionId,
        generation: join.generation,
        nonce: join.nonce,
      });
      this.#requireOpenConnection(connection);
      const currentManifest = await this.store.readManifest(this.manifest.sessionId);
      this.#requireOpenConnection(connection);
      this.#assertManifestOwnership(currentManifest, new Set(["ready", "connected"]));
      if (join.generation !== currentManifest.generation || join.generation <= this.generation) {
        throw new ProtocolError("stale_generation", "join token is not for the current unpublished generation");
      }
      await this.#authenticate(connection, join);
      return;
    }

    const message = validateSignalMessage(raw, {
      expectedSessionId: this.manifest.sessionId,
      expectedNonce: connection.nonce,
      minimumGeneration: this.generation,
      allowTokenlessJoin: true,
    });
    if (!RECEIVER_TYPES.has(message.type)) throw new ProtocolError("wrong_direction", "message type is not accepted from receiver");

    if (message.type === "join") {
      if (message.token !== undefined) throw new ProtocolError("unexpected_token", "rejoin must not reuse or replace a token");
      if (message.generation !== this.generation + 1) {
        throw new ProtocolError("invalid_generation", "authenticated rejoin must advance generation by exactly one");
      }
      await this.#advanceGeneration(connection, message);
      return;
    }

    if (message.generation === this.generation + 1 && message.type === "state" && message.state === "restarting") {
      await this.cdp.receive(message);
      return;
    }
    if (message.generation !== this.generation) {
      throw new ProtocolError("stale_generation", "message generation is not current");
    }
    if (message.type === "ice") this.#countCandidate("receiver", message.candidate);
    if (message.type === "stats") {
      await this.#handleReceiverStats(connection, message);
      return;
    }
    if (message.type === "profile") {
      this.profile = message.profile;
      if (message.profile !== "fallback") this.adaptation.reset(message.profile);
    }
    await this.cdp.receive(message);
  }

  async #authenticate(connection, join) {
    this.#requireOpenConnection(connection);
    const previousReceiver = this.receiver;
    this.receiver = connection;
    if (previousReceiver && previousReceiver !== connection) {
      previousReceiver.socket.close(4001, "replaced by newer receiver");
    }
    connection.authenticated = true;
    connection.generation = join.generation;
    connection.nonce = join.nonce;
    this.generation = join.generation;
    this.nonce = join.nonce;
    this.profile = join.profile;
    this.adaptation.reset(join.profile === "fallback" ? "mini" : join.profile);
    this.receiverIceCandidates = 0;
    this.senderIceCandidates = 0;

    let senderStartAttempted = false;
    try {
      senderStartAttempted = true;
      const senderState = await this.cdp.startSender({
        protocolVersion: PROTOCOL_VERSION,
        sessionId: this.manifest.sessionId,
        generation: this.generation,
        nonce: this.nonce,
        canvasSelector: this.manifest.target.canvasSelector,
        profile: this.profile,
        fixtureLatency: this.fixtureLatency,
      });
      this.#requireCurrentConnection(connection);
      const safeJoin = { ...join };
      delete safeJoin.token;
      await this.cdp.receive(safeJoin);
      this.#requireCurrentConnection(connection);
      await this.#recordConnected();
      this.#requireCurrentConnection(connection);
      connection.joined = true;
      if (connection.joinTimer) clearTimeout(connection.joinTimer);
      sendJson(connection.socket, envelope(
        this.manifest.sessionId,
        this.generation,
        this.nonce,
        "joined",
        {
          profile: this.profile,
          source: {
            width: senderState?.sourceWidth ?? this.source.width,
            height: senderState?.sourceHeight ?? this.source.height,
          },
        },
      ));
    } catch (error) {
      if (this.receiver === connection) this.receiver = null;
      connection.authenticated = false;
      if (senderStartAttempted) await this.cdp.stopSender("receiver-authentication-aborted").catch(this.onError);
      await this.#markReadyAfterDisconnect();
      throw error;
    }
  }

  async #advanceGeneration(connection, join) {
    this.#requireCurrentConnection(connection);
    const currentGeneration = this.generation;
    this.manifest = await this.store.updateManifest(this.manifest.sessionId, (manifest) => {
      this.#assertManifestOwnership(manifest, new Set(["ready", "connected"]));
      if ((manifest.generation ?? 0) > currentGeneration) {
        throw new ProtocolError(
          "generation_reserved",
          "authenticated rejoin cannot claim a generation reserved for a replacement receiver",
        );
      }
      return {
        ...manifest,
        generation: join.generation,
        heartbeatAt: new Date(this.now()).toISOString(),
      };
    });
    this.#requireCurrentConnection(connection);
    this.generation = join.generation;
    connection.generation = join.generation;
    this.profile = join.profile;
    this.receiverIceCandidates = 0;
    this.senderIceCandidates = 0;
    await this.cdp.receive(join);
    this.#requireCurrentConnection(connection);
    await this.#recordConnected();
    this.#requireCurrentConnection(connection);
    sendJson(connection.socket, envelope(
      this.manifest.sessionId,
      this.generation,
      this.nonce,
      "joined",
      { profile: this.profile, source: this.source },
    ));
  }

  async #recordConnected() {
    this.manifest = await this.store.updateManifest(this.manifest.sessionId, (manifest) => {
      this.#assertManifestOwnership(manifest, new Set(["ready", "connected"]));
      return {
        ...manifest,
        generation: Math.max(manifest.generation ?? 0, this.generation),
        activeGeneration: this.generation,
        state: "connected",
        heartbeatAt: new Date(this.now()).toISOString(),
      };
    });
  }

  #countCandidate(direction, candidate) {
    if (candidate === null) return;
    const key = candidateKey(direction);
    this[key] += 1;
    if (this[key] > MAX_ICE_CANDIDATES) {
      throw new ProtocolError("too_many_candidates", `${direction} sent too many ICE candidates`);
    }
  }

  async #handleReceiverStats(connection, message) {
    await this.store.appendEvidence({
      sessionId: this.manifest.sessionId,
      generation: this.generation,
      receivedAt: new Date(this.now()).toISOString(),
      profile: this.profile,
      metrics: message.stats,
    });
    this.#requireCurrentConnection(connection);
    const decision = this.adaptation.sample(message.stats, this.now());
    if (!decision.accepted) return;
    if (decision.transition) {
      this.profile = decision.transition.to;
      const profile = envelope(
        this.manifest.sessionId,
        this.generation,
        this.nonce,
        "profile",
        { profile: this.profile },
      );
      await this.cdp.receive(profile);
      sendJson(this.receiver?.socket, profile);
    }
    if (decision.action === "restart-peer") {
      sendJson(this.receiver?.socket, envelope(
        this.manifest.sessionId,
        this.generation,
        this.nonce,
        "state",
        { state: "restart-required", reason: "stale-frame-age" },
      ));
    }
  }

  async handleSenderSignal(raw) {
    if (this.closed || !this.receiver?.authenticated || !this.nonce) return false;
    this.senderRateLimiter.take();
    let message;
    try {
      message = validateSignalMessage(raw, {
        expectedSessionId: this.manifest.sessionId,
        expectedNonce: this.nonce,
        expectedGeneration: this.generation,
        allowTokenlessJoin: true,
      });
    } catch (error) {
      if (error instanceof ProtocolError && ["stale_generation", "wrong_nonce"].includes(error.code)) return false;
      throw error;
    }
    if (!SENDER_TYPES.has(message.type)) throw new ProtocolError("wrong_direction", "message type is not accepted from sender");
    if (message.type === "ice") this.#countCandidate("sender", message.candidate);
    return sendJson(this.receiver.socket, message);
  }

  #handleReceiverError(connection, error) {
    this.onError(error);
    if (connection.authenticated && !connection.closed) {
      const detail = safeReason(error);
      sendJson(connection.socket, envelope(
        this.manifest.sessionId,
        this.generation,
        connection.nonce,
        "error",
        { ...detail, retryable: false },
      ));
    }
    if (!connection.closed) connection.socket.close(1008, "signaling policy violation");
  }

  #requireOpenConnection(connection) {
    if (connection.closed || connection.socket?.readyState !== 1) {
      throw new ProtocolError("connection_closed", "receiver connection closed before signaling completed");
    }
  }

  #requireCurrentConnection(connection) {
    this.#requireOpenConnection(connection);
    if (!connection.authenticated || this.receiver !== connection) {
      throw new ProtocolError("receiver_replaced", "receiver connection has been replaced");
    }
  }

  async #markReadyAfterDisconnect() {
    if (this.receiver || this.closed) return;
    this.manifest = await this.store.updateManifest(this.manifest.sessionId, (manifest) => {
      this.#assertManifestOwnership(manifest, new Set(["ready", "connected"]));
      return {
        ...manifest,
        generation: Math.max(manifest.generation ?? 0, this.generation),
        state: manifest.state === "error" ? "error" : "ready",
        heartbeatAt: new Date(this.now()).toISOString(),
      };
    });
  }

  async #cleanupClosedConnection(connection, wasCurrent) {
    if (!wasCurrent || !connection.authenticated) return;
    await this.cdp.receive(envelope(
      this.manifest.sessionId,
      connection.generation,
      connection.nonce,
      "leave",
      { reason: "receiver-disconnected" },
    )).catch(this.onError);
    await this.#markReadyAfterDisconnect();
  }

  handleIdentityLost(detail = {}) {
    return this.#enqueueSourceFailure({
      code: "target_identity_lost",
      message: "Preview target URL or canvas identity changed",
      reason: detail.reason ?? "target-identity-lost",
    });
  }

  handleSourceLost(reason = "preview-source-lost") {
    return this.#enqueueSourceFailure({
      code: "preview_source_lost",
      message: "Preview browser source disconnected",
      reason,
    });
  }

  #enqueueSourceFailure({ code, message, reason }) {
    this.operationQueue = this.operationQueue
      .then(async () => {
        if (this.closed || this.identityLost) return;
        this.identityLost = true;
        await this.cdp.stopSender(reason).catch(this.onError);
        this.manifest = await this.store.withSessionLock(this.manifest.sessionId, async () => {
          const manifest = await this.store.readManifest(this.manifest.sessionId);
          this.#assertManifestOwnership(manifest, new Set(["starting", "ready", "connected"]));
          await this.store.revokeSessionTokens(this.manifest.sessionId);
          const failed = {
            ...manifest,
            generation: Math.max(manifest.generation ?? 0, this.generation),
            state: "error",
            lastError: { code, message },
            heartbeatAt: new Date(this.now()).toISOString(),
          };
          await this.store.writeManifest(failed);
          return failed;
        });
        const receiver = this.receiver;
        this.receiver = null;
        if (receiver && !receiver.closed) {
          sendJson(receiver.socket, envelope(
            this.manifest.sessionId,
            this.generation,
            receiver.nonce,
            "error",
            { code, message, retryable: false },
          ));
          receiver.socket.close(4002, code);
        }
      })
      .catch(this.onError);
    return this.operationQueue;
  }

  async close() {
    this.closed = true;
    if (this.receiver) {
      const receiver = this.receiver;
      this.receiver = null;
      sendJson(receiver.socket, envelope(
        this.manifest.sessionId,
        this.generation,
        this.nonce,
        "leave",
        { reason: "previewd-stopped" },
      ));
      receiver.socket.close(1001, "previewd stopped");
    }
  }
}

async function readStatic(pathname) {
  const entry = STATIC_FILES.get(pathname);
  if (!entry) return null;
  const [directory, file] = entry;
  if (basename(file) !== file) return null;
  const path = resolve(directory, file);
  const [actualDirectory, actualPath] = await Promise.all([realpath(directory), realpath(path)]);
  if (!actualPath.startsWith(`${actualDirectory}/`)) return null;
  const info = await stat(actualPath);
  if (!info.isFile() || info.size > 2 * 1024 * 1024) return null;
  return {
    body: await readFile(actualPath),
    contentType: CONTENT_TYPES[extname(actualPath)] ?? "application/octet-stream",
  };
}

export function createHttpHandler({ sessionId, getState = () => "starting" }) {
  return async (request, response) => {
    const url = new URL(request.url ?? "/", "http://127.0.0.1");
    const commonHeaders = {
      "cache-control": "no-store",
      "cross-origin-opener-policy": "same-origin",
      "referrer-policy": "no-referrer",
      "x-content-type-options": "nosniff",
      "content-security-policy": "default-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self' ws://127.0.0.1:* ws://[::1]:*",
    };
    if (request.method !== "GET" && request.method !== "HEAD") {
      response.writeHead(405, { ...commonHeaders, allow: "GET, HEAD" });
      response.end();
      return;
    }
    if (url.pathname === "/healthz") {
      if (url.search) {
        response.writeHead(404, commonHeaders);
        response.end();
        return;
      }
      const body = Buffer.from(JSON.stringify({ ok: true, protocolVersion: PROTOCOL_VERSION, sessionId, state: getState() }));
      response.writeHead(200, { ...commonHeaders, "content-type": "application/json; charset=utf-8", "content-length": body.length });
      response.end(request.method === "HEAD" ? undefined : body);
      return;
    }
    let asset;
    try {
      asset = url.search ? null : await readStatic(url.pathname);
    } catch {
      asset = null;
    }
    if (!asset) {
      response.writeHead(404, commonHeaders);
      response.end();
      return;
    }
    response.writeHead(200, { ...commonHeaders, "content-type": asset.contentType, "content-length": asset.body.length });
    response.end(request.method === "HEAD" ? undefined : asset.body);
  };
}

export function validateWebSocketUpgradeRequest(request) {
  assertLoopbackHost(request.socket?.remoteAddress, "WebSocket peer");
  const url = new URL(request.url ?? "", "http://127.0.0.1");
  if (url.pathname !== "/signal" || url.search) throw new Error("invalid WebSocket route");
  if (typeof request.headers?.origin !== "string" || typeof request.headers?.host !== "string") {
    throw new Error("WebSocket same-origin headers are required");
  }
  const origin = assertLoopbackUrl(request.headers.origin, { schemes: ["http:"], field: "WebSocket origin" });
  const host = new URL(`http://${request.headers.host}`);
  assertLoopbackHost(host.hostname, "WebSocket Host");
  if (host.pathname !== "/" || host.search || host.hash || host.username || host.password) {
    throw new Error("invalid WebSocket Host");
  }
  if (origin.host !== host.host) throw new Error("WebSocket Origin must match Host");
  return { origin, path: url.pathname };
}

const TERMINAL_MANIFEST_STATES = new Set(["stopping", "stopped"]);
const START_ERROR_CODE_MAX_LENGTH = 64;
const START_ERROR_MESSAGE_MAX_LENGTH = 512;

function daemonLifecycleError(code, message) {
  const error = new Error(message);
  error.name = "PreviewDaemonError";
  error.code = code;
  return error;
}

function boundedStartError(error) {
  const rawCode = typeof error?.code === "string" ? error.code : "previewd_start_failed";
  const code = /^[a-z0-9_.-]+$/u.test(rawCode)
    ? rawCode.slice(0, START_ERROR_CODE_MAX_LENGTH)
    : "previewd_start_failed";
  const rawMessage = typeof error?.message === "string" ? error.message : "previewd failed to start";
  const message = rawMessage.replace(/[\u0000-\u001f\u007f]/gu, " ").slice(0, START_ERROR_MESSAGE_MAX_LENGTH) ||
    "previewd failed to start";
  return { code, message };
}

export class PreviewDaemon {
  constructor({
    store,
    sessionId,
    runId = null,
    host = "127.0.0.1",
    port = 0,
    onError = () => {},
    createCdp = (options) => new CdpSession(options),
    createSignaling = (options) => new SignalingSession(options),
    createHttpServer = createServer,
    loadWebSocketServer = async () => (await import("ws")).WebSocketServer,
    setIntervalFn = setInterval,
    clearIntervalFn = clearInterval,
  }) {
    this.store = store ?? new RuntimeStore();
    this.sessionId = sessionId;
    this.runId = runId;
    this.host = assertLoopbackHost(host, "signaling host");
    if (!Number.isSafeInteger(port) || port < 0 || port > 65535) {
      throw new TypeError("signaling port must be an integer from 0 through 65535");
    }
    this.port = port;
    this.onError = onError;
    this.createCdp = createCdp;
    this.createSignaling = createSignaling;
    this.createHttpServer = createHttpServer;
    this.loadWebSocketServer = loadWebSocketServer;
    this.setIntervalFn = setIntervalFn;
    this.clearIntervalFn = clearIntervalFn;
    this.state = "starting";
    this.server = null;
    this.webSocketServer = null;
    this.cdp = null;
    this.signaling = null;
    this.heartbeatTimer = null;
  }

  #reportError(error) {
    try {
      this.onError(error);
    } catch {
      // Cleanup and terminal-state publication must not depend on an observer.
    }
  }

  #assertCurrentRun(manifest, allowedStates) {
    if ((manifest.runId ?? null) !== this.runId) {
      throw daemonLifecycleError("session_superseded", "previewd launch no longer owns this session");
    }
    if (allowedStates && !allowedStates.has(manifest.state)) {
      throw daemonLifecycleError("session_not_starting", `preview session is ${manifest.state ?? "invalid"}`);
    }
  }

  async #updateCurrentRun(patch, allowedStates) {
    return this.store.withSessionLock(this.sessionId, async () => {
      const current = await this.store.readManifest(this.sessionId);
      this.#assertCurrentRun(current, allowedStates);
      const updated = {
        ...current,
        ...patch,
        heartbeatAt: new Date(this.store.now()).toISOString(),
      };
      await this.store.writeManifest(updated);
      return updated;
    });
  }

  async #publishStartError(error) {
    const lastError = boundedStartError(error);
    try {
      await this.store.withSessionLock(this.sessionId, async () => {
        const current = await this.store.readManifest(this.sessionId);
        if ((current.runId ?? null) !== this.runId) return current;
        if (TERMINAL_MANIFEST_STATES.has(current.state) || current.state === "error") return current;
        await this.store.revokeSessionTokens(this.sessionId).catch((revokeError) => this.#reportError(revokeError));
        const failed = {
          ...current,
          state: "error",
          daemon: null,
          signaling: null,
          lastError,
          heartbeatAt: new Date(this.store.now()).toISOString(),
        };
        await this.store.writeManifest(failed);
        return failed;
      });
    } catch (publicationError) {
      this.#reportError(publicationError);
    }
  }

  async #closeResources() {
    if (this.heartbeatTimer !== null) {
      const timer = this.heartbeatTimer;
      this.heartbeatTimer = null;
      try {
        this.clearIntervalFn(timer);
      } catch (error) {
        this.#reportError(error);
      }
    }

    const signaling = this.signaling;
    this.signaling = null;
    if (signaling) await signaling.close().catch((error) => this.#reportError(error));

    const webSocketServer = this.webSocketServer;
    this.webSocketServer = null;
    if (webSocketServer) {
      for (const client of webSocketServer.clients ?? []) {
        try {
          client.terminate();
        } catch (error) {
          this.#reportError(error);
        }
      }
      await new Promise((resolvePromise) => {
        try {
          webSocketServer.close((error) => {
            if (error) this.#reportError(error);
            resolvePromise();
          });
        } catch (error) {
          this.#reportError(error);
          resolvePromise();
        }
      });
    }

    const server = this.server;
    this.server = null;
    if (server?.listening) {
      await new Promise((resolvePromise) => {
        server.close((error) => {
          if (error) this.#reportError(error);
          resolvePromise();
        });
      });
    }

    const cdp = this.cdp;
    this.cdp = null;
    if (cdp) await cdp.close().catch((error) => this.#reportError(error));
  }

  async start() {
    try {
      await this.store.initialize();
      let manifest = await this.store.readManifest(this.sessionId);
      if (this.runId === null) this.runId = manifest.runId ?? null;
      this.#assertCurrentRun(manifest, new Set(["starting"]));
      const config = await this.store.readPrivateConfig(this.sessionId);
      let signaling;
      this.cdp = this.createCdp({
        cdpUrl: config.cdpUrl,
        targetId: manifest.target.id,
        runId: this.runId,
        urlPattern: config.urlPattern,
        canvasSelector: manifest.target.canvasSelector,
        onSignal: (raw) => signaling?.handleSenderSignal(raw),
        onIdentityLost: (detail) => {
          this.state = "error";
          return signaling?.handleIdentityLost(detail);
        },
        onSourceLost: (reason) => {
          this.state = "error";
          return signaling?.handleSourceLost(reason);
        },
        onError: (error) => this.#reportError(error),
      });
      const dimensions = await this.cdp.attach();
      manifest = await this.#updateCurrentRun({
        target: { ...manifest.target, sourceWidth: dimensions.width, sourceHeight: dimensions.height },
        daemon: { pid: process.pid },
        state: "starting",
      }, new Set(["starting"]));
      signaling = this.createSignaling({
        store: this.store,
        manifest,
        cdp: this.cdp,
        fixtureLatency: config.fixtureLatency === true,
        onError: (error) => this.#reportError(error),
      });
      this.signaling = signaling;

      this.server = this.createHttpServer(createHttpHandler({ sessionId: this.sessionId, getState: () => this.state }));
      const WebSocketServer = await this.loadWebSocketServer();
      this.webSocketServer = new WebSocketServer({ noServer: true, maxPayload: MAX_SIGNAL_BYTES, perMessageDeflate: false });
      this.server.on("upgrade", (request, socket, head) => {
        try {
          validateWebSocketUpgradeRequest(request);
          if (this.webSocketServer.clients.size >= 4) throw new Error("too many pending WebSocket clients");
          this.webSocketServer.handleUpgrade(request, socket, head, (webSocket) => signaling.accept(webSocket));
        } catch {
          socket.write("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
          socket.destroy();
        }
      });

      await new Promise((resolvePromise, reject) => {
        const onError = (error) => reject(error);
        this.server.once("error", onError);
        this.server.listen(this.port, this.host, () => {
          this.server.off("error", onError);
          resolvePromise();
        });
      });
      const address = this.server.address();
      this.port = typeof address === "object" && address ? address.port : this.port;
      this.heartbeatTimer = this.setIntervalFn(() => {
        void this.#updateCurrentRun({ daemon: { pid: process.pid } }, new Set(["ready", "connected"]))
          .catch((error) => this.#reportError(error));
      }, 5_000);
      this.heartbeatTimer.unref?.();
      manifest = await this.#updateCurrentRun({
        daemon: { pid: process.pid },
        signaling: { host: this.host, port: this.port, path: "/signal" },
        state: "ready",
        lastError: null,
      }, new Set(["starting"]));
      this.signaling.manifest = manifest;
      this.state = "ready";
      return manifest;
    } catch (error) {
      this.state = "error";
      await this.#closeResources();
      await this.#publishStartError(error);
      throw error;
    }
  }

  async stop() {
    if (["stopping", "stopped"].includes(this.state)) return;
    this.state = "stopping";
    await this.#closeResources();
    this.state = "stopped";
    try {
      await this.store.withSessionLock(this.sessionId, async () => {
        const current = await this.store.readManifest(this.sessionId);
        if ((current.runId ?? null) !== this.runId) return current;
        await this.store.revokeSessionTokens(this.sessionId);
        const stopped = {
          ...current,
          state: "stopped",
          daemon: null,
          signaling: null,
          heartbeatAt: new Date(this.store.now()).toISOString(),
        };
        await this.store.writeManifest(stopped);
        return stopped;
      });
    } catch (error) {
      this.#reportError(error);
    }
  }
}

function parseDaemonArgs(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 1) {
    const name = argv[index];
    if (!["--session", "--run-id", "--runtime-dir", "--host", "--port"].includes(name)) throw new Error(`unknown argument: ${name}`);
    const value = argv[index + 1];
    if (value === undefined) throw new Error(`${name} requires a value`);
    options[name.slice(2)] = value;
    index += 1;
  }
  if (!options.session) throw new Error("--session is required");
  return options;
}

export async function runPreviewDaemon(argv = process.argv.slice(2)) {
  const options = parseDaemonArgs(argv);
  const daemon = new PreviewDaemon({
    store: new RuntimeStore({ root: options["runtime-dir"] }),
    sessionId: options.session,
    runId: options["run-id"] ?? null,
    host: options.host ?? "127.0.0.1",
    port: options.port === undefined ? 0 : Number(options.port),
    onError: (error) => process.stderr.write(`${JSON.stringify({ level: "error", code: error.code ?? "previewd_error", message: error.message })}\n`),
  });
  await daemon.start();
  const stop = async () => {
    await daemon.stop();
    process.exitCode = 0;
  };
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
  process.once("SIGHUP", stop);
  return daemon;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  runPreviewDaemon().catch((error) => {
    process.stderr.write(`${JSON.stringify({ level: "error", code: error.code ?? "previewd_start_failed", message: error.message })}\n`);
    process.exitCode = 1;
  });
}
