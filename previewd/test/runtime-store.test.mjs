import test from "node:test";
import assert from "node:assert/strict";
import { chmod, mkdir, mkdtemp, readdir, rm, stat, utimes } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  RuntimeStore,
  RuntimeStoreError,
  canonicalEnsureKey,
  runtimeStoreDefaults,
  sessionIdForEnsureKey,
} from "../src/runtime-store.mjs";
import { describeCommand, tmuxExactPaneLookupArguments } from "../bin/previewctl.mjs";

function manifest(sessionId, heartbeatAt = new Date(0).toISOString()) {
  return {
    protocolVersion: 1,
    sessionId,
    generation: 0,
    activeGeneration: 0,
    workspace: { canonicalRoot: "/workspace", revision: null },
    tmux: { session: "wscrpt-preview-test", pane: "%1", owned: true },
    target: { id: "target", urlHash: "sha256:test", canvasSelector: "canvas#game" },
    signaling: null,
    state: "starting",
    heartbeatAt,
  };
}

async function fixture(t, options = {}) {
  const parent = await mkdtemp(join(tmpdir(), "wscrpt-previewd-test-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const store = new RuntimeStore({ root: join(parent, "runtime"), ...options });
  await store.initialize();
  return store;
}

function mode(info) {
  return info.mode & 0o777;
}

test("creates private directories and atomically publishes credential-free manifests", async (t) => {
  const store = await fixture(t);
  const sessionId = "p-0123456789abcdef";
  await store.writeManifest(manifest(sessionId));
  assert.equal(mode(await stat(store.root)), runtimeStoreDefaults.directoryMode);
  assert.equal(mode(await stat(store.sessionsDirectory)), runtimeStoreDefaults.directoryMode);
  assert.equal(mode(await stat(store.sessionPath(sessionId))), runtimeStoreDefaults.fileMode);
  assert.deepEqual(await store.readManifest(sessionId), manifest(sessionId));
  assert.deepEqual((await readdir(store.sessionsDirectory)).filter((name) => name.includes(".tmp")), []);
});

test("refuses an existing non-private runtime root without mutating it", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "wscrpt-previewd-existing-root-test-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const root = join(parent, "runtime");
  await mkdir(root, { mode: 0o755 });
  await chmod(root, 0o755);

  const store = new RuntimeStore({ root });
  await assert.rejects(
    store.initialize(),
    (error) => error instanceof RuntimeStoreError && error.code === "unsafe_permissions",
  );
  assert.equal(mode(await stat(root)), 0o755);
  assert.deepEqual(await readdir(root), []);
});

test("rejects signaling credentials anywhere in a normal manifest", async (t) => {
  const store = await fixture(t);
  await assert.rejects(
    store.writeManifest({ ...manifest("p-secret"), nested: { token: "must-not-persist" } }),
    (error) => error instanceof RuntimeStoreError && error.code === "secret_in_manifest",
  );
});

test("keeps private config separate and refuses widened private-file permissions", async (t) => {
  const store = await fixture(t);
  const sessionId = "p-private-config";
  await store.writePrivateConfig(sessionId, {
    cdpUrl: "http://127.0.0.1:9222",
    urlPattern: "http://127.0.0.1:5173/*",
  });
  assert.equal(mode(await stat(store.configPath(sessionId))), 0o600);
  await chmod(store.configPath(sessionId), 0o644);
  await assert.rejects(
    store.readPrivateConfig(sessionId),
    (error) => error instanceof RuntimeStoreError && error.code === "unsafe_permissions",
  );
});

test("one-use tokens are scoped, private, expiring, and consumed atomically", async (t) => {
  let now = 10_000;
  const store = await fixture(t, { now: () => now, tokenTtlMs: 1000 });
  const issued = await store.issueToken({ sessionId: "p-token", generation: 3 });
  assert.equal(mode(await stat(store.tokenPath(issued.token))), 0o600);
  assert.equal(issued.token.includes("="), false);
  assert.equal(issued.nonce.includes("="), false);
  assert.equal((await store.consumeToken(issued)).generation, 3);
  await assert.rejects(
    store.consumeToken(issued),
    (error) => error instanceof RuntimeStoreError && error.code === "token_unavailable",
  );

  const expired = await store.issueToken({ sessionId: "p-token", generation: 4 });
  now = 11_000;
  await assert.rejects(
    store.consumeToken(expired),
    (error) => error instanceof RuntimeStoreError && error.code === "token_expired",
  );
});

test("a wrong token scope still burns the one-use token", async (t) => {
  const store = await fixture(t, { now: () => 10_000 });
  const issued = await store.issueToken({ sessionId: "p-token-scope", generation: 2 });
  await assert.rejects(
    store.consumeToken({ ...issued, generation: 3 }),
    (error) => error instanceof RuntimeStoreError && error.code === "token_scope",
  );
  await assert.rejects(
    store.consumeToken(issued),
    (error) => error instanceof RuntimeStoreError && error.code === "token_unavailable",
  );
});

test("cleanup revokes expired tokens and marks stale heartbeats without deleting evidence", async (t) => {
  let now = 20_000;
  const store = await fixture(t, { now: () => now, tokenTtlMs: 1000, heartbeatTtlMs: 5000 });
  await store.writeManifest(manifest("p-stale", new Date(10_000).toISOString()));
  await store.issueToken({ sessionId: "p-stale", generation: 1 });
  now = 21_001;
  const cleaned = await store.cleanupExpired();
  assert.deepEqual(cleaned, { expiredTokens: 1, staleSessions: ["p-stale"] });
  assert.equal((await store.readManifest("p-stale")).state, "stale");
});

test("ensure keys are stable and session IDs contain no workspace data", () => {
  const input = {
    canonicalRoot: "/private/work/project",
    tmuxPane: "%12",
    targetId: "target-id",
    canvasSelector: "canvas#game",
  };
  const first = canonicalEnsureKey(input);
  assert.equal(first, canonicalEnsureKey(input));
  assert.notEqual(first, canonicalEnsureKey({ ...input, targetId: "another" }));
  assert.match(sessionIdForEnsureKey(first), /^p-[a-f0-9]{32}$/u);
  assert.equal(sessionIdForEnsureKey(first).includes("project"), false);
});

test("writes bounded private redacted JSONL evidence and exposes a tail summary", async (t) => {
  const store = await fixture(t);
  const sessionId = "p-evidence";
  await store.appendEvidence({
    sessionId,
    generation: 2,
    receivedAt: "2026-07-29T22:00:00.000Z",
    profile: "mini",
    metrics: {
      presentedFps: 23.9,
      decodedFps: 23.8,
      frameAgeMs: 84,
      presentationAgeMs: 41,
      maxFreezeMs: 122,
      latencyMs: 121,
      frameAgeBasis: "rvfc-media-time",
      codec: "video/H264",
      localCandidateType: "host",
      remoteCandidateType: "host",
      width: 960,
      height: 540,
      framesDecoded: 1440,
      framesDropped: 2,
      bytesReceived: 1234567,
      bitrateBps: 3980000,
      availableIncomingBitrate: 7500000,
      token: "must-be-redacted",
      arbitrary: { nested: "must-be-redacted" },
    },
  });
  await store.appendEvidence({
    sessionId,
    generation: 2,
    receivedAt: "2026-07-29T22:00:01.000Z",
    profile: "fallback",
    metrics: { presentedFps: 12, frameAgeMs: 100 },
  });
  assert.equal(mode(await stat(store.evidencePath(sessionId))), 0o600);
  const evidence = await store.readEvidence(sessionId, { tail: 1 });
  assert.equal(evidence.sampleCount, 2);
  assert.equal(evidence.firstReceivedAt, "2026-07-29T22:00:00.000Z");
  assert.equal(evidence.samples.length, 1);
  assert.deepEqual(evidence.samples[0].metrics, { presentedFps: 12, frameAgeMs: 100 });
  assert.equal(JSON.stringify(evidence).includes("must-be-redacted"), false);
  const firstMetrics = (await store.readEvidence(sessionId, { tail: 2 })).samples[0].metrics;
  assert.equal(firstMetrics.decodedFps, 23.8);
  assert.equal(firstMetrics.maxFreezeMs, 122);
  assert.equal(firstMetrics.framesDecoded, 1440);
  assert.equal(firstMetrics.bitrateBps, 3980000);
  assert.equal(firstMetrics.availableIncomingBitrate, 7500000);
  assert.equal(firstMetrics.codec, "video/H264");
  assert.equal(firstMetrics.localCandidateType, "host");
});

test("describe rewrites only the returned signaling URL to the selected SSH local port", async (t) => {
  const store = await fixture(t);
  const sessionId = "p-local-forward";
  const heartbeatAt = new Date().toISOString();
  await store.writeManifest({
    ...manifest(sessionId, heartbeatAt),
    state: "ready",
    signaling: { host: "127.0.0.1", port: 7331, path: "/signal" },
  });
  const descriptor = await describeCommand({
    session: sessionId,
    "issue-token": true,
    "local-port": "49152",
    "expected-remote-port": "7331",
    profile: "expanded-headroom",
    "runtime-dir": store.root,
    json: true,
  });
  assert.equal(descriptor.signaling.url, "ws://127.0.0.1:49152/signal");
  assert.equal(descriptor.profile, "expanded-headroom");
  assert.equal(descriptor.presentation, "expanded");
  assert.equal((await store.readManifest(sessionId)).signaling.port, 7331);
  await assert.rejects(
    describeCommand({
      session: sessionId,
      "issue-token": true,
      "local-port": "49152",
      "expected-remote-port": "7444",
      "runtime-dir": store.root,
    }),
    (error) => error.code === "signaling_changed",
  );
  await assert.rejects(
    describeCommand({
      session: sessionId,
      "issue-token": true,
      "local-port": "49152",
      "runtime-dir": store.root,
    }),
    (error) => error.code === "missing_expected_remote_port",
  );
  const latest = await store.readManifest(sessionId);
  await store.writeManifest({
    ...latest,
    signaling: { ...latest.signaling, host: "::1" },
  });
  await assert.rejects(
    describeCommand({
      session: sessionId,
      "issue-token": true,
      "local-port": "49152",
      "expected-remote-port": "7331",
      "runtime-dir": store.root,
    }),
    (error) => error.code === "signaling_changed",
  );
  await store.writeManifest(latest);
  await assert.rejects(
    describeCommand({
      session: sessionId,
      "issue-token": true,
      "local-port": "70000",
      "runtime-dir": store.root,
    }),
    (error) => error.code === "invalid_local_port",
  );
  await assert.rejects(
    describeCommand({
      session: sessionId,
      "issue-token": true,
      profile: "ultra",
      "runtime-dir": store.root,
    }),
    (error) => error.code === "invalid_profile",
  );
});

test("recovers only old same-uid private session locks and rejects unsafe locks", async (t) => {
  const store = await fixture(t);
  const staleSession = "p-stale-lock";
  const staleLock = join(store.locksDirectory, `${staleSession}.lock`);
  await mkdir(staleLock, { mode: 0o700 });
  const old = new Date(Date.now() - runtimeStoreDefaults.staleLockMs - 1000);
  await utimes(staleLock, old, old);
  assert.equal(await store.withSessionLock(staleSession, async () => "recovered"), "recovered");

  const unsafeSession = "p-unsafe-lock";
  const unsafeLock = join(store.locksDirectory, `${unsafeSession}.lock`);
  await mkdir(unsafeLock, { mode: 0o700 });
  await chmod(unsafeLock, 0o755);
  await assert.rejects(
    store.withSessionLock(unsafeSession, async () => "must-not-run", { timeoutMs: 20 }),
    (error) => error instanceof RuntimeStoreError && error.code === "unsafe_permissions",
  );
});

test("tmux pane lookup targets the pane id directly without exact-session syntax", () => {
  assert.deepEqual(tmuxExactPaneLookupArguments("%12"), [
    "display-message",
    "-p",
    "-t",
    "%12",
    "#{pane_id}",
  ]);
});
