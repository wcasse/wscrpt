import test from "node:test";
import assert from "node:assert/strict";
import {
  CdpSession,
  CdpSessionError,
  assertCdpUrl,
  cdpWorldNameForRun,
  compileUrlPattern,
  requireExactCanvasInspection,
  selectExactTarget,
  urlHash,
  validateCanvasSelector,
} from "../src/cdp-session.mjs";

const targets = [
  { id: "agent-game", type: "page", title: "Game", url: "http://127.0.0.1:5173/game?session=7" },
  { id: "other", type: "page", title: "Other", url: "http://127.0.0.1:5173/other" },
];

function cdpCode(operation, code) {
  assert.throws(operation, (error) => error instanceof CdpSessionError && error.code === code);
}

test("selects only the exact target ID and expected safe URL glob", () => {
  const selected = selectExactTarget(targets, {
    targetId: "agent-game",
    urlPattern: "http://127.0.0.1:5173/game*",
  });
  assert.equal(selected.id, "agent-game");
  assert.equal(compileUrlPattern("https://example.test/a?b=1").test("https://example.test/a?b=1"), true);
  assert.match(urlHash(selected.url), /^sha256:[a-f0-9]{64}$/u);
});

test("fails closed for absent, duplicated, wrong-type, and URL-mismatched targets", () => {
  cdpCode(() => selectExactTarget(targets, { targetId: "missing", urlPattern: "*" }), "target_missing");
  cdpCode(
    () => selectExactTarget([...targets, { ...targets[0] }], { targetId: "agent-game", urlPattern: "*" }),
    "target_ambiguous",
  );
  cdpCode(
    () => selectExactTarget([{ id: "worker", type: "worker", url: "http://127.0.0.1/" }], { targetId: "worker", urlPattern: "*" }),
    "wrong_target_type",
  );
  cdpCode(
    () => selectExactTarget(targets, { targetId: "agent-game", urlPattern: "http://127.0.0.1:5173/other" }),
    "target_url_mismatch",
  );
});

test("requires exactly one capturable canvas with positive intrinsic dimensions", () => {
  assert.deepEqual(requireExactCanvasInspection({ ok: true, count: 1, width: 1280, height: 720 }), {
    width: 1280,
    height: 720,
  });
  cdpCode(() => requireExactCanvasInspection({ ok: false, count: 0, reason: "match-count" }), "canvas_missing");
  cdpCode(() => requireExactCanvasInspection({ ok: false, count: 2, reason: "match-count" }), "canvas_ambiguous");
  cdpCode(() => requireExactCanvasInspection({ ok: false, count: 1, reason: "not-canvas" }), "invalid_canvas");
  cdpCode(() => requireExactCanvasInspection({ ok: true, count: 1, width: 0, height: 720 }), "invalid_canvas_dimensions");
});

test("rejects empty selectors and every non-loopback or path-bearing CDP endpoint", () => {
  assert.equal(validateCanvasSelector("canvas#game"), "canvas#game");
  cdpCode(() => validateCanvasSelector(""), "invalid_canvas_selector");
  assert.equal(assertCdpUrl("http://127.0.0.1:9222").port, "9222");
  cdpCode(() => assertCdpUrl("http://0.0.0.0:9222"), "non_loopback");
  cdpCode(() => assertCdpUrl("http://192.168.1.5:9222"), "non_loopback");
  cdpCode(() => assertCdpUrl("http://127.0.0.1:9222/json"), "invalid_cdp_url");
});

test("revalidates URL and exact canvas after a main-frame load before resuming", async () => {
  let currentUrl = "http://127.0.0.1:5173/game";
  let canvas = { ok: true, count: 1, width: 1280, height: 720 };
  let loadHandler;
  let navigationHandler;
  let withinDocumentHandler;
  let disconnectHandler;
  let bindingHandler;
  let bindingOptions;
  let installedScriptOptions;
  const expressions = [];
  const outbound = [];
  const identityLosses = [];
  const sourceLosses = [];
  const errors = [];
  const Runtime = {
    enable: async () => {},
    addBinding: async (options) => { bindingOptions = options; },
    bindingCalled: (handler) => { bindingHandler = handler; },
    evaluate: async ({ expression, contextId }) => {
      expressions.push(expression);
      if (contextId !== undefined) assert.equal(contextId, 17);
      if (expression === "globalThis.location.href") return { result: { value: currentUrl } };
      if (expression.includes("document.querySelectorAll")) return { result: { value: canvas } };
      if (expression.includes("sender.snapshot")) {
        return { result: { value: { version: 1, state: "ready" } } };
      }
      return { result: { value: true } };
    },
  };
  const Page = {
    enable: async () => {},
    getFrameTree: async () => ({
      frameTree: { frame: { id: "main-frame", url: currentUrl } },
    }),
    createIsolatedWorld: async () => ({ executionContextId: 17 }),
    addScriptToEvaluateOnNewDocument: async (options) => { installedScriptOptions = options; },
    loadEventFired: (handler) => { loadHandler = handler; },
    frameNavigated: (handler) => { navigationHandler = handler; },
    navigatedWithinDocument: (handler) => { withinDocumentHandler = handler; },
  };
  const session = new CdpSession({
    cdpUrl: "http://127.0.0.1:9222",
    targetId: "agent-game",
    urlPattern: "http://127.0.0.1:5173/game*",
    canvasSelector: "canvas#game",
    scriptSource: "(() => {})();",
    targetLister: async () => [{ id: "agent-game", type: "page", url: currentUrl }],
    cdpFactory: async () => ({
      Runtime,
      Page,
      on(event, handler) { if (event === "disconnect") disconnectHandler = handler; },
      close: async () => {},
    }),
    onSignal: (payload) => outbound.push(payload),
    onIdentityLost: (detail) => identityLosses.push(detail),
    onSourceLost: (reason) => sourceLosses.push(reason),
    onError: (error) => errors.push(error),
  });
  assert.deepEqual(await session.attach(), { width: 1280, height: 720 });
  assert.deepEqual(bindingOptions, {
    name: "__wscrptPreviewBridge",
    executionContextName: "wscrpt-preview-v1",
  });
  assert.equal(installedScriptOptions.worldName, "wscrpt-preview-v1");
  bindingHandler({ name: "__wscrptPreviewBridge", executionContextId: 99, payload: "spoof" });
  bindingHandler({ name: "__wscrptPreviewBridge", executionContextId: 17, payload: "trusted" });
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  assert.deepEqual(outbound, ["trusted"]);
  assert.deepEqual(await session.snapshotSender(), { version: 1, state: "ready" });
  const senderConfig = {
    protocolVersion: 1,
    sessionId: "p-navigation",
    generation: 1,
    nonce: "abcdefghijklmnop",
    canvasSelector: "canvas#game",
    profile: "mini",
  };
  await session.startSender(senderConfig);
  await session.receive({
    protocolVersion: 1,
    sessionId: "p-navigation",
    generation: 1,
    nonce: "abcdefghijklmnop",
    type: "join",
    role: "receiver",
    profile: "mini",
  });
  await session.receive({
    protocolVersion: 1,
    sessionId: "p-navigation",
    generation: 1,
    nonce: "abcdefghijklmnop",
    type: "profile",
    profile: "expanded",
  });

  const receivesBeforeReload = expressions.filter((expression) => expression.includes("const receive")).length;
  navigationHandler({ frame: { id: "main-frame", url: currentUrl } });
  loadHandler();
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  assert.equal(
    expressions.filter((expression) => expression.includes("const receive")).length,
    receivesBeforeReload + 1,
  );
  const reloadStart = expressions.filter((expression) => expression.includes("sender.start")).at(-1);
  const reloadJoin = expressions.filter((expression) => expression.includes("const receive")).at(-1);
  assert.equal(reloadStart.includes('"profile":"expanded"'), true);
  assert.equal(reloadJoin.includes('"profile":"expanded"'), true);

  currentUrl = "http://127.0.0.1:5173/admin";
  navigationHandler({ frame: { id: "main-frame", url: currentUrl } });
  loadHandler();
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  assert.equal(errors.at(-1)?.code, "target_url_mismatch");

  currentUrl = "http://127.0.0.1:5173/game";
  canvas = { ok: false, count: 2, reason: "match-count" };
  const startsBeforeMissingCanvas = expressions.filter((expression) => expression.includes("sender.start")).length;
  navigationHandler({ frame: { id: "main-frame", url: currentUrl } });
  loadHandler();
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  assert.equal(errors.at(-1)?.code, "canvas_ambiguous");
  assert.deepEqual(identityLosses, [{
    code: "target_identity_lost",
    reason: "url-mismatch",
  }]);
  assert.equal(
    expressions.filter((expression) => expression.includes("sender.start")).length,
    startsBeforeMissingCanvas,
  );
  withinDocumentHandler({ frameId: "main-frame", url: "http://127.0.0.1:5173/escape" });
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  assert.equal(errors.at(-1)?.code, "target_url_mismatch");
  disconnectHandler();
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  assert.deepEqual(sourceLosses, ["cdp-disconnected"]);
  await session.close();
});

test("delayed old-run close cannot stop the replacement sender isolated world", async () => {
  const currentUrl = "http://127.0.0.1:5173/game";
  const worldContexts = new Map();
  const evaluations = [];
  const installedWorlds = [];
  const bindingWorlds = [];
  let nextContextId = 40;

  function client(owner) {
    const Runtime = {
      enable: async () => {},
      addBinding: async ({ executionContextName }) => { bindingWorlds.push({ owner, executionContextName }); },
      bindingCalled: () => {},
      evaluate: async ({ expression, contextId }) => {
        evaluations.push({ owner, expression, contextId });
        if (expression === "globalThis.location.href") return { result: { value: currentUrl } };
        if (expression.includes("document.querySelectorAll")) {
          return { result: { value: { ok: true, count: 1, width: 960, height: 540 } } };
        }
        return { result: { value: true } };
      },
    };
    const Page = {
      enable: async () => {},
      getFrameTree: async () => ({ frameTree: { frame: { id: "main-frame", url: currentUrl } } }),
      createIsolatedWorld: async ({ worldName }) => {
        if (!worldContexts.has(worldName)) worldContexts.set(worldName, nextContextId++);
        return { executionContextId: worldContexts.get(worldName) };
      },
      addScriptToEvaluateOnNewDocument: async ({ worldName }) => { installedWorlds.push({ owner, worldName }); },
      loadEventFired: () => {},
      frameNavigated: () => {},
      navigatedWithinDocument: () => {},
    };
    return { Runtime, Page, on() {}, async close() {} };
  }

  function session(owner, runId) {
    return new CdpSession({
      cdpUrl: "http://127.0.0.1:9222",
      targetId: "agent-game",
      runId,
      urlPattern: `${currentUrl}*`,
      canvasSelector: "canvas#game",
      scriptSource: "(() => {})();",
      targetLister: async () => [{ id: "agent-game", type: "page", url: currentUrl }],
      cdpFactory: async () => client(owner),
    });
  }

  const oldRunId = "run-old";
  const newRunId = "run-new";
  const oldSession = session("old", oldRunId);
  const newSession = session("new", newRunId);
  await oldSession.attach();
  await newSession.attach();
  await newSession.startSender({
    protocolVersion: 1,
    sessionId: "p-replacement",
    generation: 1,
    nonce: "abcdefghijklmnop",
    canvasSelector: "canvas#game",
    profile: "mini",
  });

  assert.notEqual(oldSession.executionContextId, newSession.executionContextId);
  assert.equal(oldSession.worldName, cdpWorldNameForRun(oldRunId));
  assert.equal(newSession.worldName, cdpWorldNameForRun(newRunId));
  assert.notEqual(oldSession.worldName, newSession.worldName);
  assert.deepEqual(installedWorlds, [
    { owner: "old", worldName: oldSession.worldName },
    { owner: "new", worldName: newSession.worldName },
  ]);
  assert.deepEqual(bindingWorlds, [
    { owner: "old", executionContextName: oldSession.worldName },
    { owner: "new", executionContextName: newSession.worldName },
  ]);

  await oldSession.close();
  const delayedStops = evaluations.filter(({ expression }) => expression.includes("sender.stop"));
  assert.equal(delayedStops.length, 1);
  assert.equal(delayedStops[0].owner, "old");
  assert.equal(delayedStops[0].contextId, oldSession.executionContextId);
  assert.notEqual(delayedStops[0].contextId, newSession.executionContextId);
  assert.equal(
    evaluations.some(({ expression, contextId }) => expression.includes("sender.stop") && contextId === newSession.executionContextId),
    false,
  );
  await newSession.close();
});
