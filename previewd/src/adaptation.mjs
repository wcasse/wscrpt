export const QUALITY_PROFILES = Object.freeze({
  mini: Object.freeze({ width: 960, height: 540, fps: 24, maxBitrate: 4_000_000 }),
  expanded: Object.freeze({ width: 1280, height: 720, fps: 24, maxBitrate: 6_000_000 }),
  "expanded-headroom": Object.freeze({ width: 1280, height: 720, fps: 30, maxBitrate: 8_000_000 }),
  fallback: Object.freeze({ width: 960, height: 540, fps: 12, maxBitrate: 1_500_000 }),
});

export const DEFAULT_ADAPTATION_LIMITS = Object.freeze({
  minimumPresentedFpsRatio: 0.8,
  maximumFrameAgeMs: 250,
  maximumPacketLossRatio: 0.05,
  maximumRttMs: 200,
  staleFrameAgeMs: 500,
});

function finite(value) {
  return Number.isFinite(value);
}

export function classifyAdaptationSample(sample, {
  targetProfile = "mini",
  limits = DEFAULT_ADAPTATION_LIMITS,
} = {}) {
  if (!QUALITY_PROFILES[targetProfile]) throw new TypeError(`unknown quality profile: ${targetProfile}`);
  if (sample === null || typeof sample !== "object") {
    return { bad: true, reasons: ["missing_sample"], stale: false };
  }

  const reasons = [];
  const expectedFps = QUALITY_PROFILES[targetProfile].fps;
  if (!finite(sample.presentedFps)) reasons.push("missing_presented_fps");
  else if (sample.presentedFps < expectedFps * limits.minimumPresentedFpsRatio) reasons.push("presented_fps");

  if (finite(sample.frameAgeMs) && sample.frameAgeMs > limits.maximumFrameAgeMs) reasons.push("frame_age");
  if (finite(sample.packetLossRatio) && sample.packetLossRatio > limits.maximumPacketLossRatio) reasons.push("packet_loss");
  if (finite(sample.rttMs) && sample.rttMs > limits.maximumRttMs) reasons.push("rtt");

  return {
    bad: reasons.length > 0,
    reasons,
    stale: finite(sample.frameAgeMs) && sample.frameAgeMs > limits.staleFrameAgeMs,
  };
}

export function normalizeAdaptationSample(sample) {
  if (sample === null || typeof sample !== "object") return sample;
  let packetLossRatio = sample.packetLossRatio;
  if (!finite(packetLossRatio) && finite(sample.packetLossDelta)) {
    const denominator = finite(sample.packetsReceived) && sample.packetsReceived > 0
      ? sample.packetsReceived + Math.max(0, sample.packetLossDelta)
      : null;
    packetLossRatio = denominator ? Math.max(0, sample.packetLossDelta) / denominator : undefined;
  }
  return {
    ...sample,
    presentedFps: finite(sample.presentedFps) ? sample.presentedFps : sample.framesPerSecond,
    frameAgeMs: finite(sample.frameAgeMs) ? sample.frameAgeMs : sample.presentationAgeMs,
    packetLossRatio,
    rttMs: finite(sample.rttMs)
      ? sample.rttMs
      : finite(sample.currentRoundTripTime)
        ? sample.currentRoundTripTime * 1_000
        : undefined,
  };
}

export class AdaptationController {
  constructor({
    primaryProfile = "mini",
    badSamplesToFallback = 3,
    goodSamplesToRecover = 10,
    staleSamplesToRestart = 3,
    sampleIntervalMs = 1_000,
    limits = DEFAULT_ADAPTATION_LIMITS,
  } = {}) {
    if (!QUALITY_PROFILES[primaryProfile] || primaryProfile === "fallback") {
      throw new TypeError("primaryProfile must be a non-fallback quality profile");
    }
    this.primaryProfile = primaryProfile;
    this.profile = primaryProfile;
    this.badSamplesToFallback = badSamplesToFallback;
    this.goodSamplesToRecover = goodSamplesToRecover;
    this.staleSamplesToRestart = staleSamplesToRestart;
    this.sampleIntervalMs = sampleIntervalMs;
    this.limits = limits;
    this.badCount = 0;
    this.goodCount = 0;
    this.staleCount = 0;
    this.lastSampleAt = -Infinity;
    this.restartLatched = false;
  }

  sample(sample, at = Date.now()) {
    if (!Number.isFinite(at)) throw new TypeError("sample time must be finite");
    if (at - this.lastSampleAt < this.sampleIntervalMs) {
      return { accepted: false, profile: this.profile, action: null };
    }
    this.lastSampleAt = at;
    const normalized = normalizeAdaptationSample(sample);
    const assessment = classifyAdaptationSample(normalized, {
      targetProfile: this.profile === "fallback" ? "fallback" : this.primaryProfile,
      limits: this.limits,
    });

    this.staleCount = assessment.stale ? this.staleCount + 1 : 0;
    let restart = false;
    if (this.staleCount >= this.staleSamplesToRestart && !this.restartLatched) {
      restart = true;
      this.restartLatched = true;
    } else if (!assessment.stale) {
      this.restartLatched = false;
    }

    let transition = null;
    if (this.profile === "fallback") {
      if (assessment.bad) {
        this.goodCount = 0;
      } else {
        this.goodCount += 1;
        if (this.goodCount >= this.goodSamplesToRecover) {
          transition = { from: "fallback", to: this.primaryProfile, reason: "sustained_recovery" };
          this.profile = this.primaryProfile;
          this.goodCount = 0;
          this.badCount = 0;
        }
      }
    } else if (assessment.bad) {
      this.goodCount = 0;
      this.badCount += 1;
      if (this.badCount >= this.badSamplesToFallback) {
        transition = { from: this.profile, to: "fallback", reason: assessment.reasons.join(",") };
        this.profile = "fallback";
        this.badCount = 0;
      }
    } else {
      this.badCount = 0;
    }

    return {
      accepted: true,
      profile: this.profile,
      assessment,
      sample: normalized,
      transition,
      action: restart ? "restart-peer" : transition ? "set-profile" : null,
    };
  }

  reset(primaryProfile = this.primaryProfile) {
    if (!QUALITY_PROFILES[primaryProfile] || primaryProfile === "fallback") {
      throw new TypeError("primaryProfile must be a non-fallback quality profile");
    }
    this.primaryProfile = primaryProfile;
    this.profile = primaryProfile;
    this.badCount = 0;
    this.goodCount = 0;
    this.staleCount = 0;
    this.lastSampleAt = -Infinity;
    this.restartLatched = false;
  }
}
