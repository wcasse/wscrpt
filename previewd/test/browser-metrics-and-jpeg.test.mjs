import assert from "node:assert/strict";
import test from "node:test";

import { LatestJpegDecoder } from "../public/jpeg-provider.mjs";
import {
  FixtureLatencyProbe,
  LatestValue,
  MetricsPump,
  PresentedFrameMetrics,
  extractReceiverStats,
} from "../public/metrics.mjs";

const tick = () => new Promise((resolve) => setImmediate(resolve));

test("latest-value handoff discards stale generations and retains one value", () => {
  const latest = new LatestValue();
  assert.equal(latest.publish({ id: "new" }, 4), true);
  assert.equal(latest.publish({ id: "old" }, 3), false);
  assert.deepEqual(latest.peek(), { id: "new" });
  assert.deepEqual(latest.take(), { id: "new" });
  assert.equal(latest.take(), undefined);
});

test("presented-frame metrics use compositor callbacks and expose frame age", () => {
  let now = 1_000;
  let callback;
  const video = {
    videoWidth: 960,
    videoHeight: 540,
    requestVideoFrameCallback(next) {
      callback = next;
      return 1;
    },
    cancelVideoFrameCallback() {},
  };
  const metrics = new PresentedFrameMetrics(video, { now: () => now });
  metrics.start();
  now = 1_020;
  callback(now, {
    width: 960,
    height: 540,
    mediaTime: 0.02,
    captureTime: 900,
  });
  now = 2_020;
  const sample = metrics.sample();
  assert.equal(sample.presentedFrames, 1);
  assert.equal(sample.presentedFps, 1 / 1.02);
  assert.equal(sample.presentationAgeMs, 1_000);
  assert.equal(sample.frameAgeMs, 1_120);
  assert.equal(sample.frameAgeBasis, "captureTime");
  assert.equal(sample.maxFreezeMs, 1_000);
  assert.equal(sample.callbackSupported, true);
  metrics.stop();
});

test("presented-frame freeze evidence resets at each sample boundary", () => {
  let now = 1_000;
  const metrics = new PresentedFrameMetrics({}, { now: () => now });
  metrics.start();
  metrics.observeFrame(1_020);
  metrics.observeFrame(1_220);
  now = 1_230;
  assert.equal(metrics.sample().maxFreezeMs, 200);
  metrics.observeFrame(1_240);
  now = 1_250;
  assert.equal(metrics.sample().maxFreezeMs, 20);
  metrics.stop();
});

test("receiver stats are compact, delta-based, and omit candidate addresses", () => {
  const report = new Map([
    [
      "in",
      {
        id: "in",
        type: "inbound-rtp",
        kind: "video",
        bytesReceived: 2_000,
        packetsReceived: 20,
        packetsLost: 2,
        framesDecoded: 30,
        frameWidth: 960,
        frameHeight: 540,
        codecId: "codec",
      },
    ],
    ["transport", { id: "transport", type: "transport", selectedCandidatePairId: "pair" }],
    [
      "pair",
      {
        id: "pair",
        type: "candidate-pair",
        state: "succeeded",
        currentRoundTripTime: 0.04,
        localCandidateId: "secret-local",
        remoteCandidateId: "secret-remote",
      },
    ],
    ["codec", { id: "codec", type: "codec", mimeType: "video/H264", payloadType: 102 }],
    [
      "secret-local",
      {
        id: "secret-local",
        type: "local-candidate",
        candidateType: "host",
        address: "192.0.2.10",
      },
    ],
    [
      "secret-remote",
      {
        id: "secret-remote",
        type: "remote-candidate",
        candidateType: "relay",
        address: "198.51.100.20",
      },
    ],
  ]);
  const previous = {
    sampledAt: 1_000,
    bytesReceived: 1_000,
    packetsLost: 1,
    framesDecoded: 10,
  };
  const stats = extractReceiverStats(report, previous, 2_000);
  assert.equal(stats.bitrateBps, 8_000);
  assert.equal(stats.decodedFps, 20);
  assert.equal(stats.packetLossDelta, 1);
  assert.equal(stats.rttMs, 40);
  assert.equal(stats.codec, "video/H264");
  assert.equal(stats.localCandidateType, "host");
  assert.equal(stats.remoteCandidateType, "relay");
  assert.equal(JSON.stringify(stats).includes("secret-"), false);
  assert.equal(JSON.stringify(stats).includes("192.0.2.10"), false);
});

test("metrics pump permits at most one getStats call in flight", async () => {
  let resolveStats;
  let calls = 0;
  const connection = {
    sampleStats() {
      calls += 1;
      return new Promise((resolve) => {
        resolveStats = resolve;
      });
    },
  };
  const pump = new MetricsPump({
    connection,
    presented: { sample: () => ({ presentedFps: 24 }) },
    profile: "mini",
    generation: 2,
  });
  const first = pump.sample();
  const second = await pump.sample();
  assert.equal(second, undefined);
  assert.equal(calls, 1);
  resolveStats({ width: 960, height: 540 });
  assert.equal((await first).presentedFps, 24);
});

test("latency probe is one-at-a-time and resolves only on the requested patch", () => {
  let now = 100;
  let messageListener;
  const sent = [];
  const channel = {
    readyState: "open",
    addEventListener(_type, listener) {
      messageListener = listener;
    },
    removeEventListener() {},
    send(value) {
      sent.push(JSON.parse(value));
    },
  };
  let pixel = [0, 0, 0, 255];
  const results = [];
  const probe = new FixtureLatencyProbe(
    {},
    {
      now: () => now,
      createCanvas: () => ({
        getContext: () => ({
          drawImage() {},
          getImageData: () => ({ data: pixel }),
        }),
      }),
      onResult: (result) => results.push(result),
    },
  );
  probe.attach(channel);
  const id = probe.request();
  assert.throws(() => probe.request(), /already outstanding/);
  messageListener({ data: JSON.stringify({ type: "latency-flash-armed", id }) });
  assert.equal(probe.observePresentedFrame(), null);
  pixel = [...sent[0].rgb, 255];
  now = 175;
  assert.deepEqual(probe.observePresentedFrame(), { id, latencyMs: 75 });
  assert.deepEqual(results, [{ id, latencyMs: 75 }]);
});

test("a lost fixture flash times out so the next latency probe can run", () => {
  let timeoutCallback;
  const sent = [];
  const probe = new FixtureLatencyProbe(
    {},
    {
      createCanvas: () => null,
      timeoutMs: 25,
      setTimeoutFn(callback, milliseconds) {
        assert.equal(milliseconds, 25);
        timeoutCallback = callback;
        return 1;
      },
      clearTimeoutFn() {},
    },
  );
  probe.attach({
    readyState: "open",
    addEventListener() {},
    removeEventListener() {},
    send(value) {
      sent.push(JSON.parse(value));
    },
  });
  assert.equal(probe.request(), "probe-1");
  assert.throws(() => probe.request(), /already outstanding/);
  timeoutCallback();
  assert.equal(probe.request(), "probe-2");
  assert.equal(sent.length, 2);
  probe.detach();
});

test("JPEG decoder drops a decoded frame superseded during decode", async () => {
  const decoders = new Map();
  const rendered = [];
  const closed = [];
  const decoder = new LatestJpegDecoder({
    decodeFrame: ({ sequence }) =>
      new Promise((resolve) => {
        decoders.set(sequence, resolve);
      }),
    renderFrame: (_image, frame) => rendered.push(frame.sequence),
  });
  decoder.offer({ sequence: 1, data: "one" });
  await tick();
  decoder.offer({ sequence: 2, data: "two" });
  decoders.get(1)({ close: () => closed.push(1) });
  await tick();
  decoders.get(2)({ close: () => closed.push(2) });
  await tick();
  assert.deepEqual(rendered, [2]);
  assert.deepEqual(closed, [1, 2]);
  assert.equal(decoder.stats().pendingFrames, 0);
  assert.equal(decoder.stats().droppedSuperseded >= 1, true);
});
