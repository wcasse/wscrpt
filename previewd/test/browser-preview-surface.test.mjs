import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  PreviewSurface,
  configurePreviewVideo,
  parseAttachFragment,
} from "../public/preview-surface.mjs";

class FakeElement {
  constructor(tagName) {
    this.tagName = tagName.toUpperCase();
    this.dataset = {};
    this.style = {};
    this.attributes = new Map();
    this.children = [];
    this.textContent = "";
    this.srcObject = null;
    this.playCalls = 0;
    this.pauseCalls = 0;
  }

  append(...children) {
    this.children.push(...children);
  }

  setAttribute(name, value) {
    this.attributes.set(name, value);
  }

  removeAttribute(name) {
    this.attributes.delete(name);
  }

  async play() {
    this.playCalls += 1;
  }

  pause() {
    this.pauseCalls += 1;
  }
}

const documentRef = {
  createElement: (tagName) => new FakeElement(tagName),
};

function descriptor() {
  return {
    protocolVersion: 1,
    sessionId: "surface-session",
    generation: 1,
    nonce: "surface_nonce_1234567890",
    token: "token_1234567890abcdefghijklmnop",
    signaling: { url: "ws://127.0.0.1:7331/signal" },
  };
}

function providerHarness(name) {
  const harness = {
    name,
    closed: 0,
    profiles: [],
    reportedStats: [],
    stateListener: null,
    stream: {
      id: `${name}-stream`,
      getTracks: () => [],
    },
  };
  harness.provider = {
    async connect() {
      return {
        stream: harness.stream,
        setProfile: async (profile) => harness.profiles.push(profile),
        sampleStats: async () => ({ width: 960, height: 540 }),
        restart: async () => {},
        reportStats: (stats) => harness.reportedStats.push(stats),
        close: () => (harness.closed += 1),
        onState: (listener) => {
          harness.stateListener = listener;
          return () => (harness.stateListener = null);
        },
        getGeneration: () => 1,
      };
    },
  };
  return harness;
}

test("preview video is muted, inline, autoplaying, control-free, and view-only", () => {
  const video = configurePreviewVideo(new FakeElement("video"));
  assert.equal(video.autoplay, true);
  assert.equal(video.muted, true);
  assert.equal(video.defaultMuted, true);
  assert.equal(video.playsInline, true);
  assert.equal(video.controls, false);
  assert.equal(video.tabIndex, -1);
  assert.equal(video.attributes.has("controls"), false);
  assert.equal(video.style.pointerEvents, "none");
});

test("surface assigns provider stream directly and tears down replaced generations", async () => {
  const root = new FakeElement("main");
  const first = providerHarness("first");
  const second = providerHarness("second");
  const messages = [];
  const surface = new PreviewSurface(root, {
    documentRef,
    providers: { first: first.provider, second: second.provider },
    onMessage: (message) => messages.push(message),
  });

  await surface.open({ session: descriptor(), provider: "first", profile: "mini" });
  assert.equal(surface.video.srcObject, first.stream);
  first.stateListener({ state: "playing" });
  assert.equal(surface.state, "playing");

  await surface.setPresentation("expanded");
  assert.deepEqual(first.profiles, ["expanded"]);
  assert.equal(surface.frame.dataset.presentation, "expanded");

  await surface.open({ session: descriptor(), provider: "second", profile: "mini" });
  assert.equal(first.closed, 1);
  assert.equal(surface.video.srcObject, second.stream);
  surface.close();
  assert.equal(second.closed, 1);
  assert.equal(surface.video.srcObject, null);
  assert.equal(messages.at(-1).state, "closed");
});

test("surface rejects unsupported presentation and provider choices", async () => {
  const surface = new PreviewSurface(new FakeElement("main"), {
    documentRef,
    providers: {},
  });
  await assert.rejects(
    surface.open({ session: descriptor(), provider: "missing", profile: "mini" }),
    /provider is unavailable/,
  );
  await assert.rejects(
    surface.open({ session: descriptor(), provider: "missing", profile: "cinema" }),
    /unknown preview profile/,
  );
  surface.close();
});

test("surface persists complete redacted evidence and each latency probe once", async () => {
  const harness = providerHarness("evidence");
  const surface = new PreviewSurface(new FakeElement("main"), {
    documentRef,
    providers: { evidence: harness.provider },
    metricsIntervalMs: 1_000_000,
  });
  await surface.open({ session: descriptor(), provider: "evidence", profile: "mini" });
  surface.lastLatencyMs = 73.1;
  surface.pendingLatencyMs = 73.1;
  const sample = {
    generation: 1,
    presentedFps: 23.9,
    decodedFps: 24,
    presentationAgeMs: 12,
    frameAgeMs: 18,
    frameAgeBasis: "receiveTime",
    maxFreezeMs: 92,
    packetsReceived: 100,
    packetsReceivedDelta: 98,
    packetsLost: 2,
    packetLossDelta: 2,
    framesDecoded: 240,
    framesDropped: 1,
    bytesReceived: 400_000,
    bitrateBps: 3_200_000,
    availableIncomingBitrate: 8_000_000,
    jitterSeconds: 0.004,
    codec: "video/H264",
    codecPayloadType: 102,
    rttMs: 21,
    width: 960,
    height: 540,
    localCandidateType: "host",
    remoteCandidateType: "host",
  };

  surface.handleMetrics(sample, surface.lifecycle);
  assert.equal(harness.reportedStats.length, 0, "startup samples do not drive adaptation");
  harness.stateListener({ state: "playing" });
  surface.handleMetrics(sample, surface.lifecycle);
  assert.deepEqual(harness.reportedStats[0], {
    presentedFps: 23.9,
    decodedFps: 24,
    frameAgeMs: 18,
    presentationAgeMs: 12,
    frameAgeBasis: "receiveTime",
    maxFreezeMs: 92,
    packetLossRatio: 0.02,
    packetLossDelta: 2,
    packetsReceived: 100,
    packetsReceivedDelta: 98,
    packetsLost: 2,
    framesDecoded: 240,
    framesDropped: 1,
    bytesReceived: 400_000,
    bitrateBps: 3_200_000,
    availableIncomingBitrate: 8_000_000,
    jitterSeconds: 0.004,
    codec: "video/H264",
    codecPayloadType: 102,
    rttMs: 21,
    latencyMs: 73.1,
    width: 960,
    height: 540,
    localCandidateType: "host",
    remoteCandidateType: "host",
    profile: "mini",
  });
  surface.handleMetrics(sample, surface.lifecycle);
  assert.equal(Object.hasOwn(harness.reportedStats[1], "latencyMs"), false);
  assert.equal(surface.latestMetrics.peek().latencyMs, 73.1, "the UI retains the latest result");
  surface.close();
  assert.equal(surface.lastLatencyMs, null, "a detached session cannot leak latency into the next one");
});

test("receiver shell has no controls and loads only the local module", async () => {
  const html = await readFile(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(html, /data-preview-root/);
  assert.match(html, /src="\/preview-surface\.mjs"/);
  assert.match(html, /ws:\/\/127\.0\.0\.1:\*/);
  assert.doesNotMatch(html, /<input|<button|<form/i);
  assert.doesNotMatch(html, /https?:\/\/(?!127\.0\.0\.1)/i);
});

test("canonical attach fragment decodes the exact Swift receiver descriptor", () => {
  const config = {
    ...descriptor(),
    profile: "mini",
    provider: "webrtc",
    presentation: "mini",
  };
  const encoded = Buffer.from(JSON.stringify(config)).toString("base64url");
  assert.deepEqual(parseAttachFragment(`#attach=${encoded}`), config);
  assert.throws(() => parseAttachFragment(`#attach=${encoded}&token=leak`));
  const withExtra = Buffer.from(JSON.stringify({ ...config, remoteHost: "remotehost" })).toString(
    "base64url",
  );
  assert.throws(() => parseAttachFragment(`#attach=${withExtra}`), /unsupported/);
});
