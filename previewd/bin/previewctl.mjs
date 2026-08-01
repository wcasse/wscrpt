#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import {
  RuntimeStore,
  canonicalEnsureKey,
  sessionIdForEnsureKey,
} from "../src/runtime-store.mjs";
import {
  assertCdpUrl,
  listCdpTargets,
  publicTargetSummary,
  selectExactTarget,
  urlHash,
  validateCanvasSelector,
} from "../src/cdp-session.mjs";
import { PROTOCOL_VERSION, assertLoopbackHost } from "../src/protocol.mjs";

const DAEMON_PATH = fileURLToPath(new URL("../src/previewd.mjs", import.meta.url));
const PACKAGE_DIRECTORY = resolve(dirname(DAEMON_PATH), "..");
const DEFAULT_CDP_URL = "http://127.0.0.1:9222";
const READY_TIMEOUT_MS = 12_000;
const LIFECYCLE_LOCK_TIMEOUT_MS = READY_TIMEOUT_MS + 1_000;
const TMUX_PANE = /^%[0-9]+$/u;
const LIVE_SESSION_STATES = new Set(["ready", "connected"]);
const ACTIVE_SESSION_STATES = new Set(["starting", ...LIVE_SESSION_STATES]);
const TERMINAL_SESSION_STATES = new Set(["stopping", "stopped"]);

class CliError extends Error {
  constructor(code, message, exitCode = 1) {
    super(message);
    this.name = "CliError";
    this.code = code;
    this.exitCode = exitCode;
  }
}

function fail(code, message, exitCode) {
  throw new CliError(code, message, exitCode);
}

function parseArguments(argv) {
  const command = argv[0];
  if (!command || command.startsWith("-")) fail("usage", "a command is required", 2);
  const options = Object.create(null);
  const flags = new Set(["json", "issue-token", "fixture-latency"]);
  for (let index = 1; index < argv.length; index += 1) {
    const argument = argv[index];
    if (!argument.startsWith("--")) fail("usage", `unexpected positional argument: ${argument}`, 2);
    const name = argument.slice(2);
    if (!name || Object.hasOwn(options, name)) fail("usage", `duplicate or invalid option: ${argument}`, 2);
    if (flags.has(name)) {
      options[name] = true;
      continue;
    }
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) fail("usage", `${argument} requires a value`, 2);
    options[name] = value;
    index += 1;
  }
  return { command, options };
}

function onlyOptions(options, allowed) {
  for (const option of Object.keys(options)) {
    if (!allowed.includes(option)) fail("usage", `unknown option: --${option}`, 2);
  }
}

function requireOption(options, name) {
  const value = options[name];
  if (typeof value !== "string" || value.length === 0) fail("usage", `--${name} is required`, 2);
  return value;
}

function tmux(arguments_) {
  return spawnSync("tmux", arguments_, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}

function tmuxAvailable() {
  const result = tmux(["-V"]);
  return !result.error && result.status === 0;
}

function tmuxHasSession(name) {
  const result = tmux(["has-session", "-t", `=${name}`]);
  return !result.error && result.status === 0;
}

export function tmuxExactPaneLookupArguments(pane) {
  return ["display-message", "-p", "-t", pane, "#{pane_id}"];
}

function tmuxHasExactPane(pane) {
  const result = tmux(tmuxExactPaneLookupArguments(pane));
  return !result.error && result.status === 0 && result.stdout.trim() === pane;
}

const DEFAULT_TMUX_ADAPTER = Object.freeze({
  available: tmuxAvailable,
  hasExactPane: tmuxHasExactPane,
  hasSession: tmuxHasSession,
  run: tmux,
});

function tmuxSessionName(sessionId) {
  return `wscrpt-preview-${sessionId.slice(2, 14)}`;
}

function gitRevision(workspace) {
  const result = spawnSync("git", ["-C", workspace, "rev-parse", "--verify", "HEAD"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  });
  if (result.status !== 0) return null;
  const revision = result.stdout.trim();
  return /^[a-f0-9]{40,64}$/u.test(revision) ? revision : null;
}

function stableError(error) {
  return {
    ok: false,
    error: {
      code: error?.code ?? "previewctl_error",
      message: error instanceof CliError || error?.name?.endsWith("Error")
        ? error.message
        : "previewctl failed",
    },
  };
}

function output(value) {
  process.stdout.write(`${JSON.stringify(value, null, 2)}\n`);
}

function runtimeStore(options) {
  return new RuntimeStore(options["runtime-dir"] ? { root: options["runtime-dir"] } : {});
}

function commandDependencies(options, overrides = {}) {
  return {
    store: overrides.store ?? runtimeStore(options),
    tmux: overrides.tmux ?? DEFAULT_TMUX_ADAPTER,
    listTargets: overrides.listCdpTargets ?? listCdpTargets,
    revision: overrides.gitRevision ?? gitRevision,
    readyTimeoutMs: overrides.readyTimeoutMs ?? READY_TIMEOUT_MS,
    lifecycleLockTimeoutMs: overrides.lifecycleLockTimeoutMs ?? LIFECYCLE_LOCK_TIMEOUT_MS,
  };
}

function sessionHealth(store, manifest, tmuxAdapter = DEFAULT_TMUX_ADAPTER) {
  const tmuxAlive = Boolean(
    manifest.tmux?.owned && manifest.tmux?.session && tmuxAdapter.hasSession(manifest.tmux.session),
  );
  const heartbeatFresh = store.isHeartbeatFresh(manifest);
  return {
    ...manifest,
    health: {
      heartbeatFresh,
      tmuxAlive,
      active: heartbeatFresh && tmuxAlive && ACTIVE_SESSION_STATES.has(manifest.state),
    },
  };
}

async function waitForReady(store, sessionId, tmuxName, {
  tmuxAdapter = DEFAULT_TMUX_ADAPTER,
  timeoutMs = READY_TIMEOUT_MS,
  lockTimeoutMs = LIFECYCLE_LOCK_TIMEOUT_MS,
} = {}) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const snapshot = await store.withSessionLock(
      sessionId,
      async () => ({
        manifest: await store.readManifest(sessionId),
        tmuxAlive: tmuxAdapter.hasSession(tmuxName),
      }),
      { timeoutMs: lockTimeoutMs },
    );
    const { manifest } = snapshot;
    if (LIVE_SESSION_STATES.has(manifest.state)) return manifest;
    if (manifest.state === "error") {
      fail(manifest.lastError?.code ?? "daemon_start_failed", manifest.lastError?.message ?? "previewd failed to start");
    }
    if (TERMINAL_SESSION_STATES.has(manifest.state)) {
      fail("session_stopped", "preview session was stopped before becoming ready");
    }
    if (manifest.state === "stale") fail("session_stale", "preview session became stale before becoming ready");
    if (!snapshot.tmuxAlive) fail("daemon_exited", "previewd tmux session exited before becoming ready");
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
  }
  fail("daemon_start_timeout", "previewd did not become ready before the timeout");
}

async function requireMatchingActiveConfiguration(store, sessionId, options, { cdpUrl, fixtureLatency }) {
  let existingConfig;
  try {
    existingConfig = await store.readPrivateConfig(sessionId);
  } catch {
    fail("configuration_missing", "active preview session is missing its private configuration");
  }
  const sameCdp = assertCdpUrl(existingConfig.cdpUrl).origin === cdpUrl;
  const samePattern = options["url-pattern"] === undefined || existingConfig.urlPattern === options["url-pattern"];
  const sameFixtureMode = existingConfig.fixtureLatency === fixtureLatency;
  if (!sameCdp || !samePattern || !sameFixtureMode) {
    fail("configuration_mismatch", "active preview session was ensured with different private options");
  }
}

export async function targetsCommand(options) {
  onlyOptions(options, ["cdp", "json", "runtime-dir"]);
  const cdpUrl = options.cdp ?? DEFAULT_CDP_URL;
  assertCdpUrl(cdpUrl);
  const targets = await listCdpTargets(cdpUrl);
  return {
    protocolVersion: PROTOCOL_VERSION,
    cdp: { host: new URL(cdpUrl).hostname, port: Number(new URL(cdpUrl).port) },
    targets: targets.filter((target) => target?.type === "page").map(publicTargetSummary),
  };
}

export async function ensureCommand(options, dependencyOverrides = {}) {
  onlyOptions(options, [
    "workspace",
    "tmux-pane",
    "target-id",
    "canvas-selector",
    "url-pattern",
    "cdp",
    "runtime-dir",
    "fixture-latency",
    "json",
  ]);
  const dependencies = commandDependencies(options, dependencyOverrides);
  const { store, tmux: tmuxAdapter } = dependencies;
  const canonicalRoot = await store.canonicalWorkspace(requireOption(options, "workspace"));
  const tmuxPane = requireOption(options, "tmux-pane");
  if (!TMUX_PANE.test(tmuxPane)) fail("invalid_tmux_pane", "--tmux-pane must be an exact tmux pane id such as %12");
  if (!tmuxAdapter.available()) fail("tmux_unavailable", "tmux is required to persist previewd");
  if (!tmuxAdapter.hasExactPane(tmuxPane)) fail("tmux_pane_missing", "the exact agent tmux pane does not exist");
  const targetId = requireOption(options, "target-id");
  const canvasSelector = validateCanvasSelector(requireOption(options, "canvas-selector"));
  const cdpUrl = assertCdpUrl(options.cdp ?? DEFAULT_CDP_URL).origin;
  const fixtureLatency = options["fixture-latency"] === true;
  const ensureKey = canonicalEnsureKey({ canonicalRoot, tmuxPane, targetId, canvasSelector });
  const sessionId = sessionIdForEnsureKey(ensureKey);
  const previewTmux = tmuxSessionName(sessionId);

  const lifecycle = await store.withSessionLock(sessionId, async () => {
    const existing = await store.readManifest(sessionId, { required: false });
    let replaceStaleOwnedTmux = false;
    if (existing && existing.ensureKey !== ensureKey) fail("session_collision", "deterministic session identity collided");
    if (existing) {
      if (existing.tmux?.owned !== true || existing.tmux?.session !== previewTmux) {
        fail("invalid_session_ownership", "existing manifest does not own the deterministic preview tmux session");
      }
      const health = sessionHealth(store, existing, tmuxAdapter);
      if (health.health.active) {
        await requireMatchingActiveConfiguration(store, sessionId, options, { cdpUrl, fixtureLatency });
        return LIVE_SESSION_STATES.has(existing.state)
          ? { action: "return", manifest: health }
          : { action: "wait" };
      }
      if (existing.state === "stopping") fail("session_stopping", "preview session is currently stopping");
      replaceStaleOwnedTmux = health.health.tmuxAlive;
    }

    const targets = await dependencies.listTargets(cdpUrl);
    const discovered = targets.filter((target) => target?.id === targetId);
    if (discovered.length !== 1) {
      selectExactTarget(targets, { targetId, urlPattern: options["url-pattern"] ?? "*" });
    }
    const urlPattern = options["url-pattern"] ?? discovered[0].url;
    const target = selectExactTarget(targets, { targetId, urlPattern });
    if (replaceStaleOwnedTmux) {
      const killed = tmuxAdapter.run(["kill-session", "-t", `=${previewTmux}`]);
      if ((killed.error || killed.status !== 0) && tmuxAdapter.hasSession(previewTmux)) {
        fail("stale_session", "could not replace stale preview-owned tmux session");
      }
    }

    await store.revokeSessionTokens(sessionId);
    await store.writePrivateConfig(sessionId, { cdpUrl, urlPattern, fixtureLatency });
    const runId = randomUUID();
    const manifest = {
      protocolVersion: PROTOCOL_VERSION,
      sessionId,
      ensureKey,
      runId,
      generation: existing?.generation ?? 0,
      activeGeneration: existing?.activeGeneration ?? 0,
      workspace: { canonicalRoot, revision: dependencies.revision(canonicalRoot) },
      tmux: { session: previewTmux, pane: tmuxPane, owned: true },
      target: {
        id: targetId,
        urlHash: urlHash(target.url),
        canvasSelector,
        sourceWidth: null,
        sourceHeight: null,
      },
      signaling: null,
      state: "starting",
      heartbeatAt: new Date(store.now()).toISOString(),
    };
    await store.writeManifest(manifest);

    const arguments_ = [
      "new-session",
      "-d",
      "-s",
      previewTmux,
      "-c",
      PACKAGE_DIRECTORY,
      process.execPath,
      DAEMON_PATH,
      "--session",
      sessionId,
      "--run-id",
      runId,
      "--runtime-dir",
      store.root,
    ];
    const started = tmuxAdapter.run(arguments_);
    if (started.error || started.status !== 0) {
      await store.writeManifest({
        ...manifest,
        state: "error",
        lastError: { code: "tmux_start_failed", message: "could not start previewd tmux session" },
      });
      fail("tmux_start_failed", "could not start previewd tmux session");
    }
    return { action: "wait" };
  }, { timeoutMs: dependencies.lifecycleLockTimeoutMs });

  if (lifecycle.action === "return") return lifecycle.manifest;
  const ready = await waitForReady(store, sessionId, previewTmux, {
    tmuxAdapter,
    timeoutMs: dependencies.readyTimeoutMs,
    lockTimeoutMs: dependencies.lifecycleLockTimeoutMs,
  });
  return sessionHealth(store, ready, tmuxAdapter);
}

export async function listCommand(options) {
  onlyOptions(options, ["workspace", "runtime-dir", "json"]);
  const store = runtimeStore(options);
  const canonicalRoot = options.workspace ? await store.canonicalWorkspace(options.workspace) : undefined;
  const manifests = await store.listManifests({ canonicalRoot });
  return { protocolVersion: PROTOCOL_VERSION, sessions: manifests.map((manifest) => sessionHealth(store, manifest)) };
}

export async function statusCommand(options) {
  onlyOptions(options, ["session", "runtime-dir", "json"]);
  const store = runtimeStore(options);
  const manifest = await store.readManifest(requireOption(options, "session"));
  return sessionHealth(store, manifest);
}

export async function describeCommand(options, dependencyOverrides = {}) {
  onlyOptions(options, ["session", "issue-token", "local-port", "expected-remote-port", "profile", "presentation", "runtime-dir", "json"]);
  const dependencies = commandDependencies(options, dependencyOverrides);
  const { store } = dependencies;
  const sessionId = requireOption(options, "session");
  const profiles = new Set(["mini", "expanded", "expanded-headroom", "fallback"]);
  const profile = options.profile ?? "mini";
  if (!profiles.has(profile)) fail("invalid_profile", "--profile is not a supported preview profile");
  const derivedPresentation = ["expanded", "expanded-headroom"].includes(profile) ? "expanded" : "mini";
  const presentation = options.presentation ?? derivedPresentation;
  if (!["mini", "expanded"].includes(presentation)) {
    fail("invalid_presentation", "--presentation must be mini or expanded");
  }
  let localPort;
  if (options["local-port"] !== undefined) {
    if (!/^[0-9]{1,5}$/u.test(options["local-port"])) fail("invalid_local_port", "--local-port must be an integer from 1 through 65535");
    localPort = Number(options["local-port"]);
    if (localPort < 1 || localPort > 65535) fail("invalid_local_port", "--local-port must be an integer from 1 through 65535");
  }
  let expectedRemotePort;
  if (options["expected-remote-port"] !== undefined) {
    if (!/^[0-9]{1,5}$/u.test(options["expected-remote-port"])) {
      fail("invalid_expected_remote_port", "--expected-remote-port must be an integer from 1 through 65535");
    }
    expectedRemotePort = Number(options["expected-remote-port"]);
    if (expectedRemotePort < 1 || expectedRemotePort > 65535) {
      fail("invalid_expected_remote_port", "--expected-remote-port must be an integer from 1 through 65535");
    }
  }
  if (!options["issue-token"]) return store.readManifest(sessionId);
  if (localPort !== undefined && expectedRemotePort === undefined) {
    fail("missing_expected_remote_port", "--expected-remote-port is required with --local-port");
  }
  const attached = await store.withSessionLock(sessionId, async () => {
    const latest = await store.readManifest(sessionId);
    if (!LIVE_SESSION_STATES.has(latest.state) || !store.isHeartbeatFresh(latest)) {
      fail("session_unavailable", "session must be live before issuing an attach token");
    }
    const signalingHost = assertLoopbackHost(latest.signaling?.host, "signaling host");
    const signalingPort = latest.signaling?.port;
    if (!Number.isSafeInteger(signalingPort) || signalingPort < 1 || signalingPort > 65535) {
      fail("invalid_signaling", "session does not have a valid signaling port");
    }
    if (expectedRemotePort !== undefined && signalingPort !== expectedRemotePort) {
      fail("signaling_changed", "session signaling port changed before token issuance");
    }
    if (expectedRemotePort !== undefined && signalingHost !== "127.0.0.1") {
      fail("signaling_changed", "session signaling host changed before token issuance");
    }
    const generation = Math.max(0, latest.generation ?? 0) + 1;
    await store.revokeSessionTokens(sessionId);
    const token = await store.issueToken({ sessionId, generation });
    try {
      await store.writeManifest({ ...latest, generation });
    } catch (error) {
      await store.revokeSessionTokens(sessionId).catch(() => {});
      throw error;
    }
    return { token, signalingHost, signalingPort };
  }, { timeoutMs: dependencies.lifecycleLockTimeoutMs });
  const descriptorHost = localPort === undefined ? attached.signalingHost : "127.0.0.1";
  const descriptorPort = localPort ?? attached.signalingPort;
  const host = descriptorHost.includes(":") ? `[${descriptorHost}]` : descriptorHost;
  return {
    protocolVersion: PROTOCOL_VERSION,
    sessionId,
    generation: attached.token.generation,
    nonce: attached.token.nonce,
    token: attached.token.token,
    signaling: { url: `ws://${host}:${descriptorPort}/signal` },
    profile,
    provider: "webrtc",
    presentation,
  };
}

export async function stopCommand(options, dependencyOverrides = {}) {
  onlyOptions(options, ["session", "runtime-dir", "json"]);
  const dependencies = commandDependencies(options, dependencyOverrides);
  const { store, tmux: tmuxAdapter } = dependencies;
  const sessionId = requireOption(options, "session");
  return store.withSessionLock(sessionId, async () => {
    const manifest = await store.readManifest(sessionId, { required: false });
    if (!manifest) return { protocolVersion: PROTOCOL_VERSION, sessionId, state: "stopped", alreadyStopped: true };
    let stoppedTmux = false;
    if (
      manifest.tmux?.owned === true &&
      manifest.tmux.session === tmuxSessionName(sessionId) &&
      tmuxAdapter.hasSession(manifest.tmux.session)
    ) {
      const result = tmuxAdapter.run(["kill-session", "-t", `=${manifest.tmux.session}`]);
      if (result.error || result.status !== 0) {
        if (tmuxAdapter.hasSession(manifest.tmux.session)) {
          fail("stop_failed", "could not stop preview-owned tmux session");
        }
      } else {
        stoppedTmux = true;
      }
    }
    await store.revokeSessionTokens(sessionId);
    const stopped = {
      ...manifest,
      state: "stopped",
      daemon: null,
      signaling: null,
      heartbeatAt: new Date(store.now()).toISOString(),
    };
    await store.writeManifest(stopped);
    return { ...stopped, stoppedTmux, alreadyStopped: manifest.state === "stopped" };
  }, { timeoutMs: dependencies.lifecycleLockTimeoutMs });
}

export async function evidenceCommand(options) {
  onlyOptions(options, ["session", "tail", "runtime-dir", "json"]);
  const store = runtimeStore(options);
  const sessionId = requireOption(options, "session");
  await store.readManifest(sessionId);
  const tail = options.tail === undefined ? 60 : Number(options.tail);
  if (!Number.isSafeInteger(tail)) fail("invalid_tail", "--tail must be an integer");
  return {
    protocolVersion: PROTOCOL_VERSION,
    sessionId,
    ...(await store.readEvidence(sessionId, { tail })),
  };
}

export async function runPreviewctl(argv = process.argv.slice(2)) {
  const { command, options } = parseArguments(argv);
  switch (command) {
    case "targets": return targetsCommand(options);
    case "ensure": return ensureCommand(options);
    case "list": return listCommand(options);
    case "describe": return describeCommand(options);
    case "status": return statusCommand(options);
    case "evidence": return evidenceCommand(options);
    case "stop": return stopCommand(options);
    default: fail("usage", `unknown command: ${command}`, 2);
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  runPreviewctl()
    .then(output)
    .catch((error) => {
      process.stderr.write(`${JSON.stringify(stableError(error), null, 2)}\n`);
      process.exitCode = error.exitCode ?? 1;
    });
}
