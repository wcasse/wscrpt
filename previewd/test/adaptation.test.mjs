import test from "node:test";
import assert from "node:assert/strict";
import {
  AdaptationController,
  QUALITY_PROFILES,
  classifyAdaptationSample,
  normalizeAdaptationSample,
} from "../src/adaptation.mjs";

const good24 = { presentedFps: 24, frameAgeMs: 80, packetLossRatio: 0, rttMs: 25 };
const bad24 = { presentedFps: 10, frameAgeMs: 300, packetLossRatio: 0.1, rttMs: 300 };
const good12 = { presentedFps: 12, frameAgeMs: 80, packetLossRatio: 0, rttMs: 25 };

test("quality profiles preserve the agreed dimensions, cadence, and bitrate caps", () => {
  assert.deepEqual(QUALITY_PROFILES.mini, { width: 960, height: 540, fps: 24, maxBitrate: 4_000_000 });
  assert.deepEqual(QUALITY_PROFILES.expanded, { width: 1280, height: 720, fps: 24, maxBitrate: 6_000_000 });
  assert.equal(QUALITY_PROFILES["expanded-headroom"].fps, 30);
  assert.equal(QUALITY_PROFILES.fallback.fps, 12);
});

test("enters fallback on the third one-hertz bad sample", () => {
  const controller = new AdaptationController();
  assert.equal(controller.sample(bad24, 0).transition, null);
  assert.equal(controller.sample(bad24, 1000).transition, null);
  const result = controller.sample(bad24, 2000);
  assert.deepEqual(result.transition, {
    from: "mini",
    to: "fallback",
    reason: "presented_fps,frame_age,packet_loss,rtt",
  });
  assert.equal(result.profile, "fallback");
});

test("requires ten consecutive good fallback samples before recovery", () => {
  const controller = new AdaptationController();
  [0, 1000, 2000].forEach((at) => controller.sample(bad24, at));
  for (let index = 0; index < 9; index += 1) {
    assert.equal(controller.sample(good12, 3000 + index * 1000).transition, null);
  }
  const recovered = controller.sample(good12, 12_000);
  assert.deepEqual(recovered.transition, { from: "fallback", to: "mini", reason: "sustained_recovery" });
});

test("ignores oversampling and emits one fresh-peer action per stale episode", () => {
  const controller = new AdaptationController();
  assert.equal(controller.sample(good24, 0).accepted, true);
  assert.equal(controller.sample(good24, 500).accepted, false);
  const stale = { ...bad24, frameAgeMs: 600 };
  controller.sample(stale, 1000);
  controller.sample(stale, 2000);
  assert.equal(controller.sample(stale, 3000).action, "restart-peer");
  assert.notEqual(controller.sample(stale, 4000).action, "restart-peer");
  controller.sample(good24, 5000);
  controller.sample(stale, 6000);
  controller.sample(stale, 7000);
  assert.equal(controller.sample(stale, 8000).action, "restart-peer");
});

test("normalizes WebKit/browser metric aliases and tolerates missing transport fields", () => {
  const normalized = normalizeAdaptationSample({
    presentedFps: 24,
    presentationAgeMs: 90,
    packetLossDelta: 2,
    packetsReceived: 98,
    currentRoundTripTime: 0.04,
  });
  assert.equal(normalized.frameAgeMs, 90);
  assert.equal(normalized.packetLossRatio, 0.02);
  assert.equal(normalized.rttMs, 40);
  assert.deepEqual(classifyAdaptationSample({ presentedFps: 24 }), { bad: false, reasons: [], stale: false });
});
