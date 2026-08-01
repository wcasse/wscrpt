import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import CDP from "chrome-remote-interface";

import { AdaptationController } from "../src/adaptation.mjs";
import { urlHash } from "../src/cdp-session.mjs";
import { PreviewDaemon, createHttpHandler } from "../src/previewd.mjs";
import { RuntimeStore } from "../src/runtime-store.mjs";
import {
  evaluate,
  launchPreviewChrome,
  waitForValue,
} from "./support/preview-chromium.mjs";

const chromeExecutable = process.env.WSCRPT_PREVIEW_CHROME;
const skipReason = chromeExecutable
  ? false
  : "set WSCRPT_PREVIEW_CHROME to a Chrome/Chromium executable to run the real WebRTC smoke";
const FRAME_TARGET = 36;

function closeServer(server) {
  if (!server?.listening) return Promise.resolve();
  return new Promise((resolvePromise, reject) => {
    server.close((error) => (error ? reject(error) : resolvePromise()));
  });
}

async function boundedCleanup(t, description, operation, timeoutMs = 5_000) {
  let timer;
  const result = await Promise.race([
    Promise.resolve()
      .then(operation)
      .then(
        () => ({ ok: true }),
        (error) => ({ error }),
      ),
    new Promise((resolvePromise) => {
      timer = setTimeout(() => resolvePromise({ timeout: true }), timeoutMs);
    }),
  ]);
  clearTimeout(timer);
  if (result.timeout) t.diagnostic(`cleanup timed out: ${description}`);
  if (result.error) t.diagnostic(`cleanup failed (${description}): ${result.error.message}`);
  return result.ok === true;
}

async function listenLoopback(server) {
  await new Promise((resolvePromise, reject) => {
    const onError = (error) => reject(error);
    server.once("error", onError);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", onError);
      resolvePromise();
    });
  });
  const address = server.address();
  assert(address && typeof address === "object");
  assert.equal(address.address, "127.0.0.1");
  return address.port;
}

async function connectTarget(chrome, targetId) {
  const client = await CDP({
    host: chrome.host,
    port: chrome.port,
    secure: false,
    target: targetId,
  });
  await Promise.all([client.Page.enable(), client.Runtime.enable()]);
  return client;
}

async function closeTarget(chrome, target) {
  if (!target) return;
  await CDP.Close({
    host: chrome.host,
    port: chrome.port,
    secure: false,
    id: target.id,
  }).catch(() => {});
}

function playerProbeExpression() {
  return `(() => {
    const frame = document.querySelector(".preview-frame");
    const video = document.querySelector("video.preview-video");
    return {
      readyState: document.readyState,
      state: frame?.dataset.state ?? null,
      status: document.querySelector(".preview-status")?.textContent ?? null,
      videoReadyState: video?.readyState ?? 0,
      paused: video?.paused ?? true,
      width: video?.videoWidth ?? 0,
      height: video?.videoHeight ?? 0,
      hasStream: video?.srcObject instanceof MediaStream,
      trackCount: video?.srcObject?.getVideoTracks?.().length ?? 0,
      trackId: video?.srcObject?.getVideoTracks?.()[0]?.id ?? null,
      trackMuted: video?.srcObject?.getVideoTracks?.()[0]?.muted ?? null,
      trackReadyState: video?.srcObject?.getVideoTracks?.()[0]?.readyState ?? null,
      mediaError: video?.error?.message ?? null,
    };
  })()`;
}

function receiverStatsExpression() {
  return `(async () => {
    const peers = globalThis.__wscrptE2EPeers ?? [];
    const peer = peers.at(-1);
    if (!peer) return null;
    const report = await peer.getStats();
    const values = [...report.values()];
    const inbound = values.find(
      (entry) => entry.type === "inbound-rtp" && !entry.isRemote &&
        (entry.kind === "video" || entry.mediaType === "video"),
    );
    const transport = values.find((entry) => entry.type === "transport");
    const pair = values.find(
      (entry) => entry.type === "candidate-pair" &&
        (entry.id === transport?.selectedCandidatePairId || entry.selected || entry.nominated) &&
        (!entry.state || entry.state === "succeeded"),
    );
    const local = values.find((entry) => entry.id === pair?.localCandidateId);
    const remote = values.find((entry) => entry.id === pair?.remoteCandidateId);
    const codec = values.find((entry) => entry.id === inbound?.codecId);
    return {
      sampledAt: performance.now(),
      peerCount: peers.length,
      connectionState: peer.connectionState,
      bytesReceived: inbound?.bytesReceived ?? 0,
      framesDecoded: inbound?.framesDecoded ?? 0,
      framesDropped: inbound?.framesDropped ?? 0,
      decodedFps: Number.isFinite(inbound?.framesPerSecond) ? inbound.framesPerSecond : null,
      width: inbound?.frameWidth ?? 0,
      height: inbound?.frameHeight ?? 0,
      codec: codec?.mimeType ?? null,
      rttMs: Number.isFinite(pair?.currentRoundTripTime)
        ? pair.currentRoundTripTime * 1000
        : null,
      localCandidateType: local?.candidateType ?? null,
      remoteCandidateType: remote?.candidateType ?? null,
      localProtocol: local?.protocol ?? null,
      remoteProtocol: remote?.protocol ?? null,
    };
  })()`;
}

test(
  "real daemon streams the exact clock canvas to a Chromium receiver at mini dimensions",
  { skip: skipReason, timeout: 90_000 },
  async (t) => {
    const runtimeRoot = await mkdtemp(join(tmpdir(), "wscrpt-preview-e2e-runtime-"));
    const sourceChrome = await launchPreviewChrome(chromeExecutable);
    let receiverChrome = null;
    const fixtureServer = createServer(
      createHttpHandler({ sessionId: "chromium-e2e-fixture", getState: () => "fixture" }),
    );
    let daemon = null;
    let sourceTarget = null;
    let receiverTarget = null;
    let sourceClient = null;
    let receiverClient = null;

    t.after(async () => {
      if (receiverClient) {
        await boundedCleanup(
          t,
          "receiver detach",
          () => evaluate(receiverClient, "globalThis.wscrptPreview?.detach?.()"),
          1_000,
        );
      }
      if (receiverChrome) {
        await boundedCleanup(t, "receiver target close", () =>
          closeTarget(receiverChrome, receiverTarget));
      }
      await boundedCleanup(t, "receiver CDP close", () => receiverClient?.close());
      if (!(await boundedCleanup(t, "preview daemon stop", () => daemon?.stop()))) {
        for (const client of daemon?.webSocketServer?.clients ?? []) client.terminate();
        daemon?.server?.closeAllConnections?.();
      }
      await boundedCleanup(t, "source CDP close", () => sourceClient?.close());
      await boundedCleanup(t, "source target close", () => closeTarget(sourceChrome, sourceTarget));
      fixtureServer.closeAllConnections?.();
      await boundedCleanup(t, "fixture server close", () => closeServer(fixtureServer));
      await boundedCleanup(t, "receiver Chrome close", () => receiverChrome?.close(), 8_000);
      await boundedCleanup(t, "source Chrome close", () => sourceChrome.close(), 8_000);
      await boundedCleanup(t, "temporary runtime removal", () =>
        rm(runtimeRoot, { recursive: true, force: true }));
    });

    receiverChrome = await launchPreviewChrome(chromeExecutable);

    const fixturePort = await listenLoopback(fixtureServer);
    const fixtureUrl = `http://127.0.0.1:${fixturePort}/fixtures/clock-game.html`;
    sourceTarget = await CDP.New({
      host: sourceChrome.host,
      port: sourceChrome.port,
      secure: false,
      url: fixtureUrl,
    });
    sourceClient = await connectTarget(sourceChrome, sourceTarget.id);
    await sourceClient.Page.bringToFront();

    const fixture = await waitForValue(
      "the deterministic 1280x720 source canvas",
      async () => {
        const snapshot = await evaluate(
          sourceClient,
          `(() => {
            const matches = document.querySelectorAll("canvas#clock-game");
            const state = globalThis.__wscrptPreviewFixture?.snapshot?.();
            if (document.readyState !== "complete" || matches.length !== 1 || !state) return null;
            return { ...state, matchCount: matches.length, url: location.href };
          })()`,
        );
        return snapshot?.sequence > 0 ? snapshot : null;
      },
      { timeoutMs: 10_000 },
    );
    assert.deepEqual(
      { width: fixture.width, height: fixture.height, matchCount: fixture.matchCount },
      { width: 1280, height: 720, matchCount: 1 },
    );
    assert.equal(fixture.url, fixtureUrl);

    const targets = await CDP.List({
      host: sourceChrome.host,
      port: sourceChrome.port,
      secure: false,
    });
    const exactTargets = targets.filter(
      (candidate) => candidate.id === sourceTarget.id && candidate.url === fixtureUrl,
    );
    assert.equal(exactTargets.length, 1, "the source target ID and URL must resolve exactly once");

    const repositoryRoot = await realpath(fileURLToPath(new URL("../..", import.meta.url)));
    const sessionId = "chromium-e2e";
    const store = new RuntimeStore({ root: runtimeRoot });
    await store.writePrivateConfig(sessionId, {
      cdpUrl: `http://${sourceChrome.host}:${sourceChrome.port}`,
      urlPattern: fixtureUrl,
      fixtureLatency: true,
    });
    await store.writeManifest({
      protocolVersion: 1,
      sessionId,
      ensureKey: "e2e-owned-runtime",
      generation: 0,
      activeGeneration: 0,
      workspace: { canonicalRoot: repositoryRoot, revision: "e2e" },
      tmux: { session: "e2e", pane: "%e2e", owned: false },
      target: {
        id: sourceTarget.id,
        urlHash: urlHash(fixtureUrl),
        canvasSelector: "canvas#clock-game",
        sourceWidth: null,
        sourceHeight: null,
      },
      signaling: null,
      state: "starting",
      heartbeatAt: new Date().toISOString(),
    });

    const daemonErrors = [];
    daemon = new PreviewDaemon({
      store,
      sessionId,
      host: "127.0.0.1",
      port: 0,
      onError: (error) => daemonErrors.push(error),
    });
    const ready = await daemon.start();
    // Hold the requested mini profile for this short functional/cadence smoke.
    // Default fallback transitions are covered deterministically in the unit
    // suite and on the separate impaired-route acceptance run; allowing them
    // here makes cold-start CPU load change the profile under measurement.
    daemon.signaling.adaptation = new AdaptationController({
      primaryProfile: "mini",
      badSamplesToFallback: 90,
      staleSamplesToRestart: 90,
    });
    assert.equal(daemon.server.address().address, "127.0.0.1");
    assert.equal(ready.target.id, sourceTarget.id);
    assert.equal(ready.target.canvasSelector, "canvas#clock-game");
    assert.deepEqual(
      { width: ready.target.sourceWidth, height: ready.target.sourceHeight },
      { width: 1280, height: 720 },
    );
    const senderSignals = [];
    const handleSenderSignal = daemon.signaling.handleSenderSignal.bind(daemon.signaling);
    daemon.signaling.handleSenderSignal = async (raw) => {
      try {
        const message = JSON.parse(raw);
        senderSignals.push({ type: message.type, state: message.state, stats: message.stats });
        if (senderSignals.length > 20) senderSignals.shift();
      } catch {
        // The real daemon remains responsible for protocol validation.
      }
      return handleSenderSignal(raw);
    };

    const issued = await store.withSessionLock(sessionId, async () => {
      const current = await store.readManifest(sessionId);
      const generation = current.generation + 1;
      await store.revokeSessionTokens(sessionId);
      const token = await store.issueToken({ sessionId, generation });
      await store.writeManifest({ ...current, generation });
      return token;
    });
    const descriptor = {
      protocolVersion: 1,
      sessionId,
      generation: issued.generation,
      nonce: issued.nonce,
      token: issued.token,
      signaling: { url: `ws://127.0.0.1:${daemon.port}/signal` },
      profile: "mini",
      provider: "webrtc",
      presentation: "mini",
    };
    const attach = Buffer.from(JSON.stringify(descriptor)).toString("base64url");
    receiverTarget = await CDP.New({
      host: receiverChrome.host,
      port: receiverChrome.port,
      secure: false,
      url: "about:blank",
    });
    receiverClient = await connectTarget(receiverChrome, receiverTarget.id);
    const receiverExceptions = [];
    receiverClient.Runtime.exceptionThrown((event) => {
      receiverExceptions.push(
        event.exceptionDetails?.exception?.description ??
          event.exceptionDetails?.text ??
          "unknown receiver exception",
      );
    });
    await receiverClient.Page.addScriptToEvaluateOnNewDocument({
      source: `(() => {
        const NativePeerConnection = globalThis.RTCPeerConnection;
        const peers = [];
        class TrackedPeerConnection extends NativePeerConnection {
          constructor(...arguments_) {
            super(...arguments_);
            peers.push(this);
          }
        }
        Object.defineProperty(globalThis, "RTCPeerConnection", {
          value: TrackedPeerConnection,
          configurable: false,
          writable: false,
        });
        Object.defineProperty(globalThis, "__wscrptE2EPeers", {
          value: peers,
          configurable: false,
          writable: false,
        });
        const nativeMessages = [];
        Object.defineProperty(globalThis, "__wscrptE2ENativeMessages", {
          value: nativeMessages,
          configurable: false,
          writable: false,
        });
        Object.defineProperty(globalThis, "webkit", {
          value: {
            messageHandlers: {
              preview: {
                postMessage(message) {
                  nativeMessages.push(JSON.parse(JSON.stringify(message)));
                  if (nativeMessages.length > 100) nativeMessages.shift();
                },
              },
            },
          },
          configurable: false,
          writable: false,
        });
      })();`,
    });
    const playerUrl = `http://127.0.0.1:${daemon.port}/index.html#attach=${attach}`;
    const navigation = await receiverClient.Page.navigate({ url: playerUrl });
    assert.equal(navigation.errorText, undefined);
    await receiverClient.Page.bringToFront();

    const playing = await waitForValue(
      "a playing 960x540 WebRTC video",
      async () => {
        const snapshot = await evaluate(receiverClient, playerProbeExpression());
        if (snapshot?.state === "error") {
          const daemonDetail = daemonErrors
            .map((error) => `${error?.code ?? error?.name ?? "error"}: ${error?.message ?? error}`)
            .join("; ");
          const receiverDetail = receiverExceptions.join("; ");
          throw new Error(
            [snapshot.status ?? "receiver entered an error state", daemonDetail, receiverDetail]
              .filter(Boolean)
              .join("; "),
          );
        }
        if (snapshot?.state === "playing" &&
          snapshot.hasStream &&
          snapshot.trackCount === 1 &&
          snapshot.videoReadyState >= 2 &&
          snapshot.width > 0) {
          return snapshot;
        }
        const senderState = await daemon.cdp.snapshotSender().catch(() => null);
        const fixtureState = await evaluate(
          sourceClient,
          "globalThis.__wscrptPreviewFixture?.snapshot?.()",
        );
        const signalState = {
          generation: daemon.signaling?.generation,
          receiverIceCandidates: daemon.signaling?.receiverIceCandidates,
          senderIceCandidates: daemon.signaling?.senderIceCandidates,
          receiverOpen: daemon.signaling?.receiver?.socket?.readyState,
        };
        throw new Error(
          `receiver=${JSON.stringify(snapshot)}; sender=${JSON.stringify(senderState)}; fixture=${JSON.stringify(fixtureState)}; signaling=${JSON.stringify(signalState)}; senderSignals=${JSON.stringify(senderSignals)}; daemon=${daemonErrors
            .map((error) => error?.message ?? String(error))
            .join("; ")}; exceptions=${receiverExceptions.join("; ")}`,
        );
      },
      { timeoutMs: 15_000 },
    );
    assert.deepEqual(
      { width: playing.width, height: playing.height, paused: playing.paused },
      { width: 960, height: 540, paused: false },
    );

    const installedProbe = await evaluate(
      receiverClient,
      `(() => {
        const video = document.querySelector("video.preview-video");
        if (!video || typeof video.requestVideoFrameCallback !== "function") {
          throw new Error("requestVideoFrameCallback is required for the Chromium media smoke");
        }
        const track = video.srcObject?.getVideoTracks?.()[0];
        if (!track) throw new Error("receiver has no remote video track");
        const probe = {
          active: true,
          count: 0,
          firstNow: null,
          lastNow: null,
          firstMediaTime: null,
          lastMediaTime: null,
          lastWidth: 0,
          lastHeight: 0,
          lastCaptureTime: null,
          lastReceiveTime: null,
          frameAgeBasis: null,
          lastFrameAgeMs: null,
          maxFrameAgeMs: 0,
          maxPresentationGapMs: 0,
          sourceTimedFrames: 0,
        };
        const sample = (now, metadata) => {
          if (!probe.active) return;
          if (probe.lastNow !== null) {
            probe.maxPresentationGapMs = Math.max(
              probe.maxPresentationGapMs,
              now - probe.lastNow,
            );
          }
          probe.count += 1;
          probe.firstNow ??= now;
          probe.firstMediaTime ??= metadata.mediaTime;
          probe.lastNow = now;
          probe.lastMediaTime = metadata.mediaTime;
          probe.lastWidth = metadata.width;
          probe.lastHeight = metadata.height;
          probe.lastCaptureTime = Number.isFinite(metadata.captureTime)
            ? metadata.captureTime
            : null;
          probe.lastReceiveTime = Number.isFinite(metadata.receiveTime)
            ? metadata.receiveTime
            : null;
          const sourceTime = probe.lastCaptureTime ?? probe.lastReceiveTime;
          if (sourceTime !== null) {
            const age = now - sourceTime;
            if (age >= 0 && age <= 60000) {
              probe.frameAgeBasis = probe.lastCaptureTime === null
                ? "receiveTime"
                : "captureTime";
              probe.lastFrameAgeMs = age;
              probe.maxFrameAgeMs = Math.max(probe.maxFrameAgeMs, age);
              probe.sourceTimedFrames += 1;
            }
          }
          video.requestVideoFrameCallback(sample);
        };
        video.requestVideoFrameCallback(sample);
        globalThis.__wscrptE2EFrameProbe = probe;
        return { settings: track.getSettings(), width: video.videoWidth, height: video.videoHeight };
      })()`,
    );
    assert.equal(installedProbe.width, 960);
    assert.equal(installedProbe.height, 540);

    const frameProbe = await waitForValue(
      `${FRAME_TARGET} decoded video frames`,
      async () => {
        const sample = await evaluate(
          receiverClient,
          `(() => {
            const probe = globalThis.__wscrptE2EFrameProbe;
            const video = document.querySelector("video.preview-video");
            if (!probe || !video) return null;
            return {
              ...probe,
              totalVideoFrames: video.getVideoPlaybackQuality?.().totalVideoFrames ?? 0,
              videoWidth: video.videoWidth,
              videoHeight: video.videoHeight,
              trackSettings: video.srcObject?.getVideoTracks?.()[0]?.getSettings?.() ?? {},
            };
          })()`,
        );
        return sample?.count >= FRAME_TARGET ? sample : null;
      },
      { timeoutMs: 10_000, intervalMs: 100 },
    );
    const elapsedSeconds = (frameProbe.lastNow - frameProbe.firstNow) / 1_000;
    const presentedFps = (frameProbe.count - 1) / elapsedSeconds;
    assert(frameProbe.totalVideoFrames >= FRAME_TARGET, "decoded frame counter must advance");
    assert(frameProbe.lastMediaTime > frameProbe.firstMediaTime, "media timestamps must advance");
    assert.deepEqual(
      {
        videoWidth: frameProbe.videoWidth,
        videoHeight: frameProbe.videoHeight,
        callbackWidth: frameProbe.lastWidth,
        callbackHeight: frameProbe.lastHeight,
      },
      { videoWidth: 960, videoHeight: 540, callbackWidth: 960, callbackHeight: 540 },
    );
    assert(
      presentedFps >= 18 && presentedFps <= 35,
      `expected the requested 24 FPS path, observed ${presentedFps.toFixed(1)} FPS`,
    );
    if (Number.isFinite(frameProbe.trackSettings.frameRate)) {
      // Chrome's remote-track setting is a fractional runtime estimate; the
      // frame-callback cadence above is the authoritative delivery measure.
      assert(
        frameProbe.trackSettings.frameRate >= 18 && frameProbe.trackSettings.frameRate <= 35,
        `receiver track reported ${frameProbe.trackSettings.frameRate} FPS`,
      );
    }

    const sourceProgress = await evaluate(
      sourceClient,
      "globalThis.__wscrptPreviewFixture?.snapshot?.()",
    );
    assert(
      sourceProgress.sequence > fixture.sequence + FRAME_TARGET,
      "the exact selected source canvas sequence must keep advancing",
    );
    const outboundStats = await waitForValue(
      "nonzero sender WebRTC stats",
      async () => {
        const stats = [...senderSignals]
          .reverse()
          .find((signal) => signal.type === "stats" && signal.stats?.framesEncoded > 0)
          ?.stats;
        return stats?.frameWidth > 0 ? stats : null;
      },
      { timeoutMs: 5_000, intervalMs: 100 },
    );
    assert.deepEqual(
      { width: outboundStats.frameWidth, height: outboundStats.frameHeight },
      { width: 960, height: 540 },
    );
    assert(outboundStats.framesEncoded > 0);
    assert(outboundStats.bytesSent > 0);

    const receiverStatsStart = await evaluate(receiverClient, receiverStatsExpression());
    const receiverStats = await waitForValue(
      "advancing redacted receiver stats",
      async () => {
        const stats = await evaluate(receiverClient, receiverStatsExpression());
        return stats?.sampledAt - receiverStatsStart.sampledAt >= 750 &&
          stats.framesDecoded > receiverStatsStart.framesDecoded &&
          stats.bytesReceived > receiverStatsStart.bytesReceived
          ? stats
          : null;
      },
      { timeoutMs: 5_000, intervalMs: 100 },
    );
    const receiverBitrateBps = Math.round(
      ((receiverStats.bytesReceived - receiverStatsStart.bytesReceived) * 8 * 1_000) /
        (receiverStats.sampledAt - receiverStatsStart.sampledAt),
    );
    assert.deepEqual(
      { width: receiverStats.width, height: receiverStats.height },
      { width: 960, height: 540 },
    );
    assert(receiverStats.framesDecoded >= FRAME_TARGET);
    assert(receiverBitrateBps > 0);
    assert.match(receiverStats.codec ?? "", /^video\//u);
    assert(
      ["host", "srflx", "prflx", "relay"].includes(receiverStats.localCandidateType),
      `unexpected local candidate type: ${receiverStats.localCandidateType}`,
    );
    assert(
      ["host", "srflx", "prflx", "relay"].includes(receiverStats.remoteCandidateType),
      `unexpected remote candidate type: ${receiverStats.remoteCandidateType}`,
    );
    assert.equal(Object.hasOwn(receiverStats, "address"), false);

    const noBacklog = frameProbe.frameAgeBasis
      ? {
          status: "observed",
          basis: frameProbe.frameAgeBasis,
          lastFrameAgeMs: frameProbe.lastFrameAgeMs,
          maxFrameAgeMs: frameProbe.maxFrameAgeMs,
        }
      : {
          status: "unproven",
          basis: "requestVideoFrameCallback did not expose captureTime or receiveTime",
        };
    if (noBacklog.status === "observed") {
      assert(noBacklog.lastFrameAgeMs < 500, "the latest decoded frame must not be stale");
      assert(noBacklog.maxFrameAgeMs < 1_000, "the local smoke must not accumulate old frames");
    }

    const beforeBlockedReceiver = await evaluate(
      receiverClient,
      `(() => {
        const probe = globalThis.__wscrptE2EFrameProbe;
        return { count: probe.count, mediaTime: probe.lastMediaTime };
      })()`,
    );
    await evaluate(
      receiverClient,
      `(() => {
        const blockedAt = performance.now();
        while (performance.now() - blockedAt < 750) {
          // Deliberately block only this test receiver's JS main thread.
        }
      })()`,
    );
    const impairmentClearedAt = performance.now();
    const blockedReceiverRecovery = await waitForValue(
      "fresh media after a deliberately blocked receiver",
      async () => {
        const probe = await evaluate(
          receiverClient,
          `(() => {
            const value = globalThis.__wscrptE2EFrameProbe;
            return {
              count: value.count,
              mediaTime: value.lastMediaTime,
              frameAgeBasis: value.frameAgeBasis,
              lastFrameAgeMs: value.lastFrameAgeMs,
            };
          })()`,
        );
        const ageFresh = !probe.frameAgeBasis || probe.lastFrameAgeMs < 250;
        return probe.count >= beforeBlockedReceiver.count + 3 &&
          probe.mediaTime >= beforeBlockedReceiver.mediaTime + 0.5 &&
          ageFresh
          ? probe
          : null;
      },
      { timeoutMs: 2_000, intervalMs: 50 },
    );
    const blockedReceiverEvidence = {
      blockedMs: 750,
      recoveryMs: Number((performance.now() - impairmentClearedAt).toFixed(2)),
      callbackDelta: blockedReceiverRecovery.count - beforeBlockedReceiver.count,
      mediaTimeJumpMs: Number(
        ((blockedReceiverRecovery.mediaTime - beforeBlockedReceiver.mediaTime) * 1_000).toFixed(2),
      ),
      frameAgeBasis: blockedReceiverRecovery.frameAgeBasis,
      lastFrameAgeMs: blockedReceiverRecovery.lastFrameAgeMs,
    };
    assert(
      blockedReceiverEvidence.callbackDelta < 10,
      "receiver callbacks must skip ahead instead of replaying a queued frame backlog",
    );
    assert(blockedReceiverEvidence.recoveryMs < 2_000);

    const latencyProbeId = await waitForValue(
      "the test-only latency data channel",
      () => evaluate(receiverClient, "globalThis.wscrptPreview.requestLatencyProbe()"),
      { timeoutMs: 5_000, intervalMs: 100 },
    );
    const latencyMetrics = await waitForValue(
      "a request-to-glass fixture latency sample",
      async () => {
        const metrics = await evaluate(
          receiverClient,
          `globalThis.__wscrptE2ENativeMessages
            ?.filter((message) => message.type === "metrics" &&
              Number.isFinite(message.metrics?.latencyMs))
            .at(-1)?.metrics ?? null`,
        );
        return metrics;
      },
      { timeoutMs: 8_000, intervalMs: 100 },
    );
    assert(latencyMetrics.latencyMs >= 0 && latencyMetrics.latencyMs < 1_000);
    const flashedSource = await waitForValue(
      "the exact source fixture flash identity",
      async () => {
        const snapshot = await evaluate(
          sourceClient,
          "globalThis.__wscrptPreviewFixture?.snapshot?.()",
        );
        return snapshot?.flashId === latencyProbeId ? snapshot : null;
      },
      { timeoutMs: 2_000 },
    );
    assert.deepEqual(flashedSource.flashRgb, [255, 0, 255]);

    const sender = await daemon.cdp.snapshotSender();
    assert.deepEqual(
      {
        sessionId: sender.sessionId,
        generation: sender.generation,
        profile: sender.profile,
        sourceWidth: sender.sourceWidth,
        sourceHeight: sender.sourceHeight,
        state: sender.state,
      },
      {
        sessionId,
        generation: 1,
        profile: "mini",
        sourceWidth: 1280,
        sourceHeight: 720,
        state: "connected",
      },
    );

    const beforeReload = await evaluate(
      receiverClient,
      `(() => ({
        count: globalThis.__wscrptE2EFrameProbe?.count ?? 0,
        trackId: document.querySelector("video.preview-video")
          ?.srcObject?.getVideoTracks?.()[0]?.id ?? null,
      }))()`,
    );
    const generationBeforeReload = daemon.signaling.generation;
    await sourceClient.Page.reload({ ignoreCache: true });
    const reloadedSource = await waitForValue(
      "the allowed exact source URL after reload",
      async () => {
        const snapshot = await evaluate(
          sourceClient,
          `(() => {
            const state = globalThis.__wscrptPreviewFixture?.snapshot?.();
            return state && location.href === ${JSON.stringify(fixtureUrl)}
              ? { ...state, url: location.href }
              : null;
          })()`,
        );
        return snapshot?.sequence > 5 ? snapshot : null;
      },
      { timeoutMs: 10_000 },
    );
    assert.equal(reloadedSource.url, fixtureUrl);

    const recovered = await waitForValue(
      "a fresh receiver track after allowed source reload",
      async () => {
        const player = await evaluate(receiverClient, playerProbeExpression());
        const count = await evaluate(
          receiverClient,
          "globalThis.__wscrptE2EFrameProbe?.count ?? 0",
        );
        if (player?.state === "error") throw new Error(player.status);
        return player?.state === "playing" &&
          player.trackId &&
          player.trackId !== beforeReload.trackId &&
          count >= beforeReload.count + 12 &&
          daemon.signaling.generation === generationBeforeReload
          ? { ...player, count, generation: daemon.signaling.generation }
          : null;
      },
      { timeoutMs: 25_000, intervalMs: 100 },
    );
    assert.deepEqual(
      { width: recovered.width, height: recovered.height },
      { width: 960, height: 540 },
    );
    const senderAfterReload = await daemon.cdp.snapshotSender();
    assert.equal(recovered.generation, generationBeforeReload);
    assert.equal(senderAfterReload.generation, recovered.generation);
    assert.equal(senderAfterReload.state, "connected");

    t.diagnostic(
      JSON.stringify({
        evidence: "local-chromium-functional-smoke",
        source: {
          targetId: sourceTarget.id,
          canvasSelector: "canvas#clock-game",
          width: 1280,
          height: 720,
          sequenceStart: fixture.sequence,
          sequenceEnd: sourceProgress.sequence,
        },
        receiver: {
          width: frameProbe.videoWidth,
          height: frameProbe.videoHeight,
          presentedFps: Number(presentedFps.toFixed(2)),
          decodedFps: receiverStats.decodedFps,
          framesDecoded: receiverStats.framesDecoded,
          bitrateBps: receiverBitrateBps,
          codec: receiverStats.codec,
          maxPresentationGapMs: Number(frameProbe.maxPresentationGapMs.toFixed(2)),
          localCandidateType: receiverStats.localCandidateType,
          remoteCandidateType: receiverStats.remoteCandidateType,
        },
        latency: {
          requestToGlassMs: Number(latencyMetrics.latencyMs.toFixed(2)),
          noBacklog,
          blockedReceiver: blockedReceiverEvidence,
        },
        reload: {
          generation: recovered.generation,
          freshTrack: recovered.trackId !== beforeReload.trackId,
          continuedFrames: recovered.count - beforeReload.count,
        },
      }),
    );

    await evaluate(
      receiverClient,
      `(() => {
        globalThis.__wscrptE2EFrameProbe.active = false;
        globalThis.wscrptPreview.detach();
      })()`,
    );
    const detached = await waitForValue("receiver teardown", async () => {
      const snapshot = await evaluate(receiverClient, playerProbeExpression());
      return snapshot?.state === "closed" && !snapshot.hasStream ? snapshot : null;
    });
    assert.equal(detached.trackCount, 0);
    const senderReady = await waitForValue("sender teardown", async () => {
      const snapshot = await daemon.cdp.snapshotSender();
      return snapshot?.state === "ready" ? snapshot : null;
    }, { timeoutMs: 2_000, intervalMs: 25 });
    assert.equal(senderReady.state, "ready");
    assert.equal(
      await evaluate(sourceClient, "typeof globalThis.__wscrptPreviewSender"),
      "undefined",
      "the isolated sender API must not leak into the page main world",
    );

    await daemon.stop();
    assert.equal(daemon.state, "stopped");
    assert.equal(daemon.server, null);
    assert.equal((await store.readManifest(sessionId)).state, "stopped");
    await assert.rejects(
      fetch(`http://127.0.0.1:${daemon.port}/healthz`, {
        signal: AbortSignal.timeout(1_000),
      }),
      /fetch failed|aborted|refused/i,
    );
    assert.deepEqual(daemonErrors, []);
    assert.deepEqual(receiverExceptions, []);
  },
);
