import test from "node:test";
import assert from "node:assert/strict";
import { LatestJpegFrameSlot } from "../src/cdp-jpeg-source.mjs";

function deferred() {
  let resolve;
  const promise = new Promise((resolvePromise) => { resolve = resolvePromise; });
  return { promise, resolve };
}

test("acknowledges every CDP frame while retaining only the newest undecoded frame", async () => {
  const firstDecode = deferred();
  const consumed = [];
  const acknowledged = [];
  const slot = new LatestJpegFrameSlot({
    acknowledge: async (sessionId) => acknowledged.push(sessionId),
    consume: async (frame) => {
      consumed.push(frame.sequence);
      if (frame.sequence === 1) await firstDecode.promise;
    },
  });
  slot.offer({ sequence: 1, sessionId: 101, data: "one" });
  slot.offer({ sequence: 2, sessionId: 102, data: "two" });
  slot.offer({ sequence: 3, sessionId: 103, data: "three" });
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  assert.deepEqual(acknowledged, [101, 102, 103]);
  assert.equal(slot.metrics().queued, 1);
  firstDecode.resolve();
  await slot.waitForIdle();
  assert.deepEqual(consumed, [1, 3]);
  assert.deepEqual(slot.metrics(), {
    provider: "cdp-jpeg",
    diagnostic: true,
    accepted: 3,
    acknowledged: 3,
    displayed: 2,
    dropped: 1,
    queued: 0,
  });
});

test("decode failures do not prevent the latest frame or acknowledgements", async () => {
  const consumed = [];
  const errors = [];
  const slot = new LatestJpegFrameSlot({
    acknowledge: async () => {},
    consume: async (frame) => {
      consumed.push(frame.sequence);
      if (frame.sequence === 1) throw new Error("decode failed");
    },
    onError: (error) => errors.push(error.message),
  });
  slot.offer({ sequence: 1, sessionId: 1 });
  slot.offer({ sequence: 2, sessionId: 2 });
  await slot.waitForIdle();
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  assert.deepEqual(consumed, [1, 2]);
  assert.deepEqual(errors, ["decode failed"]);
  assert.equal(slot.metrics().acknowledged, 2);
});

test("close refuses new frames and drops the one pending frame", async () => {
  const blocked = deferred();
  const slot = new LatestJpegFrameSlot({ consume: () => blocked.promise });
  assert.equal(slot.offer({ sequence: 1, sessionId: 1 }), true);
  assert.equal(slot.offer({ sequence: 2, sessionId: 2 }), true);
  slot.close();
  assert.equal(slot.offer({ sequence: 3, sessionId: 3 }), false);
  assert.equal(slot.metrics().dropped, 1);
  blocked.resolve();
  await slot.waitForIdle();
});
