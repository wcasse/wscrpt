import test from "node:test";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describeCommand, ensureCommand, stopCommand } from "../bin/previewctl.mjs";
import {
  RuntimeStore,
  RuntimeStoreError,
  canonicalEnsureKey,
  sessionIdForEnsureKey,
} from "../src/runtime-store.mjs";

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => { resolve = resolvePromise; });
  return { promise, resolve };
}

async function fixture(t) {
  const parent = await mkdtemp(join(tmpdir(), "wscrpt-previewctl-lifecycle-test-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const workspace = join(parent, "workspace");
  await mkdir(workspace);
  const store = new RuntimeStore({ root: join(parent, "runtime") });
  await store.initialize();
  return { store, workspace };
}

function fakeTmux() {
  const sessions = new Set();
  const calls = [];
  return {
    sessions,
    calls,
    adapter: {
      available: () => true,
      hasExactPane: (pane) => pane === "%12",
      hasSession: (name) => sessions.has(name),
      run: (arguments_) => {
        calls.push([...arguments_]);
        if (arguments_[0] === "new-session") {
          const name = arguments_[arguments_.indexOf("-s") + 1];
          if (sessions.has(name)) return { status: 1, error: null };
          sessions.add(name);
          return { status: 0, error: null };
        }
        if (arguments_[0] === "kill-session") {
          const name = arguments_[arguments_.indexOf("-t") + 1].replace(/^=/u, "");
          return { status: sessions.delete(name) ? 0 : 1, error: null };
        }
        return { status: 1, error: null };
      },
    },
  };
}

function ensureOptions(workspace) {
  return {
    workspace,
    "tmux-pane": "%12",
    "target-id": "clock-target",
    "canvas-selector": "canvas#game",
    cdp: "http://127.0.0.1:9222",
  };
}

async function sessionIdentity(store, workspace) {
  const canonicalRoot = await store.canonicalWorkspace(workspace);
  const ensureKey = canonicalEnsureKey({
    canonicalRoot,
    tmuxPane: "%12",
    targetId: "clock-target",
    canvasSelector: "canvas#game",
  });
  const sessionId = sessionIdForEnsureKey(ensureKey);
  return {
    canonicalRoot,
    ensureKey,
    sessionId,
    tmuxSession: `wscrpt-preview-${sessionId.slice(2, 14)}`,
  };
}

function manifest({ canonicalRoot, ensureKey, sessionId, tmuxSession, state = "ready", runId = "run-current" }) {
  return {
    protocolVersion: 1,
    sessionId,
    ensureKey,
    runId,
    generation: 0,
    activeGeneration: 0,
    workspace: { canonicalRoot, revision: null },
    tmux: { session: tmuxSession, pane: "%12", owned: true },
    target: {
      id: "clock-target",
      urlHash: "sha256:test",
      canvasSelector: "canvas#game",
      sourceWidth: 960,
      sourceHeight: 540,
    },
    signaling: state === "ready" ? { host: "127.0.0.1", port: 7331, path: "/signal" } : null,
    state,
    heartbeatAt: new Date().toISOString(),
  };
}

test("ensure waits for an existing starting launch instead of returning it as ready", async (t) => {
  const { store, workspace } = await fixture(t);
  const identity = await sessionIdentity(store, workspace);
  const tmux = fakeTmux();
  tmux.sessions.add(identity.tmuxSession);
  await store.writePrivateConfig(identity.sessionId, {
    cdpUrl: "http://127.0.0.1:9222",
    urlPattern: "http://127.0.0.1:5173/game",
    fixtureLatency: false,
  });
  await store.writeManifest(manifest({ ...identity, state: "starting" }));

  let targetLists = 0;
  const ensuring = ensureCommand(ensureOptions(workspace), {
    store,
    tmux: tmux.adapter,
    listCdpTargets: async () => { targetLists += 1; return []; },
    gitRevision: () => null,
    readyTimeoutMs: 1_000,
  });
  await new Promise((resolvePromise) => setTimeout(resolvePromise, 20));
  await store.updateManifest(identity.sessionId, (current) => ({
    ...current,
    state: "ready",
    signaling: { host: "127.0.0.1", port: 7331, path: "/signal" },
  }));

  const result = await ensuring;
  assert.equal(result.state, "ready");
  assert.equal(result.health.active, true);
  assert.equal(targetLists, 0);
  assert.equal(tmux.calls.filter((call) => call[0] === "new-session").length, 0);
});

test("a concurrent stop revokes a token issued immediately before it", async (t) => {
  const { store, workspace } = await fixture(t);
  const identity = await sessionIdentity(store, workspace);
  const tmux = fakeTmux();
  tmux.sessions.add(identity.tmuxSession);
  await store.writeManifest(manifest(identity));

  const tokenIssued = deferred();
  const releaseDescribe = deferred();
  const issueToken = store.issueToken.bind(store);
  store.issueToken = async (scope) => {
    const issued = await issueToken(scope);
    tokenIssued.resolve(issued);
    await releaseDescribe.promise;
    return issued;
  };

  const describing = describeCommand({ session: identity.sessionId, "issue-token": true }, { store });
  const issued = await tokenIssued.promise;
  const stopping = stopCommand({ session: identity.sessionId }, { store, tmux: tmux.adapter });
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  releaseDescribe.resolve();

  const [descriptor, stopped] = await Promise.all([describing, stopping]);
  assert.equal(descriptor.token, issued.token);
  assert.equal(stopped.state, "stopped");
  await assert.rejects(
    store.consumeToken(descriptor),
    (error) => error instanceof RuntimeStoreError && error.code === "token_unavailable",
  );
  assert.equal((await store.readManifest(identity.sessionId)).state, "stopped");
});

test("stop serialized behind ensure wins without a late starting overwrite", async (t) => {
  const { store, workspace } = await fixture(t);
  const identity = await sessionIdentity(store, workspace);
  const tmux = fakeTmux();
  const discoveryEntered = deferred();
  const releaseDiscovery = deferred();
  const ensuring = ensureCommand(ensureOptions(workspace), {
    store,
    tmux: tmux.adapter,
    listCdpTargets: async () => {
      discoveryEntered.resolve();
      await releaseDiscovery.promise;
      return [{ id: "clock-target", type: "page", url: "http://127.0.0.1:5173/game" }];
    },
    gitRevision: () => null,
    readyTimeoutMs: 1_000,
  });
  await discoveryEntered.promise;
  const stopping = stopCommand({ session: identity.sessionId }, { store, tmux: tmux.adapter });
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  releaseDiscovery.resolve();

  await assert.rejects(ensuring, (error) => error.code === "session_stopped");
  const stopped = await stopping;
  assert.equal(stopped.state, "stopped");
  assert.equal((await store.readManifest(identity.sessionId)).state, "stopped");
  assert.equal(tmux.sessions.has(identity.tmuxSession), false);
  assert.equal(tmux.calls.filter((call) => call[0] === "new-session").length, 1);
});

test("restarting an inactive session revokes tokens from the prior run", async (t) => {
  const { store, workspace } = await fixture(t);
  const identity = await sessionIdentity(store, workspace);
  const tmux = fakeTmux();
  await store.writeManifest(manifest({ ...identity, state: "stopped", runId: "run-old" }));
  const oldToken = await store.issueToken({ sessionId: identity.sessionId, generation: 1 });
  const originalRun = tmux.adapter.run;
  tmux.adapter.run = (arguments_) => {
    const result = originalRun(arguments_);
    if (arguments_[0] === "new-session" && result.status === 0) {
      setTimeout(() => {
        void store.updateManifest(identity.sessionId, (current) => ({
          ...current,
          state: "ready",
          signaling: { host: "127.0.0.1", port: 7331, path: "/signal" },
        }));
      }, 10);
    }
    return result;
  };

  const ready = await ensureCommand(ensureOptions(workspace), {
    store,
    tmux: tmux.adapter,
    listCdpTargets: async () => [
      { id: "clock-target", type: "page", url: "http://127.0.0.1:5173/game" },
    ],
    gitRevision: () => null,
    readyTimeoutMs: 1_000,
  });
  assert.equal(ready.state, "ready");
  assert.notEqual(ready.runId, "run-old");
  await assert.rejects(
    store.consumeToken(oldToken),
    (error) => error instanceof RuntimeStoreError && error.code === "token_unavailable",
  );
});
