import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import {
  MAX_SIGNAL_BYTES,
  FixedWindowRateLimiter,
  assertLoopbackUrl,
} from "./protocol.mjs";

export const CDP_BINDING_NAME = "__wscrptPreviewBridge";
export const CDP_RECEIVE_NAME = "__wscrptPreviewReceive";
export const CDP_SENDER_NAME = "__wscrptPreviewSender";
export const CDP_WORLD_NAME = "wscrpt-preview-v1";

const MAX_TARGETS_RESPONSE_BYTES = 1024 * 1024;
const TARGET_ID = /^[^\s\0]{1,512}$/u;

export class CdpSessionError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "CdpSessionError";
    this.code = code;
  }
}

function cdpError(code, message) {
  throw new CdpSessionError(code, message);
}

export function cdpWorldNameForRun(runId) {
  if (runId === null || runId === undefined) return CDP_WORLD_NAME;
  if (typeof runId !== "string" || runId.length < 1 || runId.length > 256 || runId.includes("\0")) {
    cdpError("invalid_run_id", "preview runId must be a bounded non-empty string");
  }
  const suffix = createHash("sha256").update(runId).digest("hex").slice(0, 32);
  return `${CDP_WORLD_NAME}-${suffix}`;
}

export function assertCdpUrl(value) {
  let url;
  try {
    url = assertLoopbackUrl(value, { schemes: ["http:"], field: "CDP URL" });
  } catch (error) {
    cdpError(error.code ?? "invalid_cdp_url", error.message);
  }
  if ((url.pathname !== "/" && url.pathname !== "") || url.search || url.hash) {
    cdpError("invalid_cdp_url", "CDP URL must contain only a loopback origin and port");
  }
  if (!url.port) cdpError("invalid_cdp_url", "CDP URL must include an explicit port");
  return url;
}

export function urlHash(value) {
  return `sha256:${createHash("sha256").update(value).digest("hex")}`;
}

export function compileUrlPattern(pattern) {
  if (typeof pattern !== "string" || pattern.length < 1 || pattern.length > 2048 || pattern.includes("\0")) {
    cdpError("invalid_url_pattern", "URL pattern must be between 1 and 2048 characters");
  }
  const escaped = pattern.replace(/[|\\{}()[\]^$+?.]/gu, "\\$&").replace(/\*/gu, ".*");
  return new RegExp(`^${escaped}$`, "u");
}

export function validateCanvasSelector(selector) {
  if (typeof selector !== "string" || selector.trim() === "" || selector.length > 512 || selector.includes("\0")) {
    cdpError("invalid_canvas_selector", "canvas selector must be between 1 and 512 characters");
  }
  return selector;
}

export function requireExactCanvasInspection(value) {
  if (!value?.ok) {
    if (value?.reason === "match-count") {
      cdpError(value.count === 0 ? "canvas_missing" : "canvas_ambiguous", "canvas selector must match exactly one element");
    }
    cdpError("invalid_canvas", `selected element cannot be captured (${value?.reason ?? "unknown"})`);
  }
  if (!Number.isSafeInteger(value.width) || value.width < 1 || !Number.isSafeInteger(value.height) || value.height < 1) {
    cdpError("invalid_canvas_dimensions", "selected canvas has invalid intrinsic dimensions");
  }
  return { width: value.width, height: value.height };
}

export function selectExactTarget(targets, { targetId, urlPattern }) {
  if (!Array.isArray(targets)) cdpError("invalid_targets", "CDP target list must be an array");
  if (typeof targetId !== "string" || !TARGET_ID.test(targetId)) {
    cdpError("invalid_target_id", "an explicit target ID is required");
  }
  const matches = targets.filter((target) => target && target.id === targetId);
  if (matches.length === 0) cdpError("target_missing", "the exact CDP target ID is not present");
  if (matches.length !== 1) cdpError("target_ambiguous", "the exact CDP target ID is ambiguous");
  const target = matches[0];
  if (target.type !== "page") cdpError("wrong_target_type", "the selected CDP target is not a page");
  if (typeof target.url !== "string" || !compileUrlPattern(urlPattern).test(target.url)) {
    cdpError("target_url_mismatch", "the selected target does not match the expected URL pattern");
  }
  return target;
}

export function publicTargetSummary(target) {
  return {
    id: target.id,
    type: target.type,
    title: typeof target.title === "string" ? target.title.slice(0, 512) : "",
    url: typeof target.url === "string" ? target.url : "",
    urlHash: typeof target.url === "string" ? urlHash(target.url) : null,
  };
}

export async function listCdpTargets(cdpUrl, { fetchImpl = globalThis.fetch, timeoutMs = 3_000 } = {}) {
  const endpoint = assertCdpUrl(cdpUrl);
  if (typeof fetchImpl !== "function") cdpError("fetch_unavailable", "fetch is not available");
  const listUrl = new URL("/json/list", endpoint);
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), timeoutMs);
  let response;
  try {
    response = await fetchImpl(listUrl, {
      redirect: "manual",
      signal: controller.signal,
      headers: { accept: "application/json" },
    });
  } catch (error) {
    cdpError("cdp_unreachable", `could not reach loopback CDP endpoint: ${error.message}`);
  } finally {
    clearTimeout(timeout);
  }
  if (!response.ok) cdpError("cdp_http_error", `CDP target list returned HTTP ${response.status}`);
  const contentLength = Number(response.headers.get("content-length"));
  if (Number.isFinite(contentLength) && contentLength > MAX_TARGETS_RESPONSE_BYTES) {
    cdpError("cdp_response_too_large", "CDP target list exceeded 1 MiB");
  }
  const text = await response.text();
  if (Buffer.byteLength(text, "utf8") > MAX_TARGETS_RESPONSE_BYTES) {
    cdpError("cdp_response_too_large", "CDP target list exceeded 1 MiB");
  }
  let targets;
  try {
    targets = JSON.parse(text);
  } catch {
    cdpError("invalid_cdp_response", "CDP target list was not valid JSON");
  }
  if (!Array.isArray(targets)) cdpError("invalid_cdp_response", "CDP target list was not an array");
  return targets;
}

export async function loadCanvasSenderScript() {
  const path = fileURLToPath(new URL("../injected/canvas-sender.mjs", import.meta.url));
  return readFile(path, "utf8");
}

export class CdpSession {
  constructor({
    cdpUrl,
    targetId,
    runId = null,
    urlPattern,
    canvasSelector,
    onSignal = async () => {},
    onError = () => {},
    onIdentityLost = async () => {},
    onSourceLost = async () => {},
    scriptSource,
    cdpFactory,
    targetLister = listCdpTargets,
  }) {
    this.endpoint = assertCdpUrl(cdpUrl);
    if (typeof targetId !== "string" || !TARGET_ID.test(targetId)) cdpError("invalid_target_id", "an explicit target ID is required");
    this.targetId = targetId;
    this.runId = runId;
    this.worldName = cdpWorldNameForRun(runId);
    this.urlPattern = urlPattern;
    this.urlRegex = compileUrlPattern(urlPattern);
    this.canvasSelector = validateCanvasSelector(canvasSelector);
    this.onSignal = onSignal;
    this.onError = onError;
    this.onIdentityLost = onIdentityLost;
    this.onSourceLost = onSourceLost;
    this.scriptSource = scriptSource;
    this.cdpFactory = cdpFactory;
    this.targetLister = targetLister;
    this.client = null;
    this.senderConfig = null;
    this.lastJoin = null;
    this.reinstalling = null;
    this.mainFrameId = null;
    this.executionContextId = null;
    this.navigationAllowed = false;
    this.bindingRateLimiter = new FixedWindowRateLimiter();
    this.resumeAfterNavigation = false;
    this.identityLostReported = false;
    this.closing = false;
  }

  async attach() {
    if (this.client) return this.inspectCanvas();
    this.closing = false;
    const targets = await this.targetLister(this.endpoint.href);
    selectExactTarget(targets, { targetId: this.targetId, urlPattern: this.urlPattern });
    const factory = this.cdpFactory ?? (await import("chrome-remote-interface")).default;
    this.client = await factory({
      host: this.endpoint.hostname,
      port: Number(this.endpoint.port),
      secure: false,
      target: this.targetId,
    });
    this.client.on?.("disconnect", () => {
      if (!this.closing) Promise.resolve(this.onSourceLost("cdp-disconnected")).catch(this.onError);
    });
    const { Runtime, Page } = this.client;
    const enables = [Runtime.enable(), Page.enable()];
    if (this.client.Inspector?.enable) enables.push(this.client.Inspector.enable());
    await Promise.all(enables);
    this.client.Inspector?.targetCrashed?.(() => {
      if (!this.closing) Promise.resolve(this.onSourceLost("target-crashed")).catch(this.onError);
    });
    const frameTree = await Page.getFrameTree();
    this.mainFrameId = frameTree.frameTree?.frame?.id;
    if (!this.mainFrameId) cdpError("main_frame_missing", "CDP did not report a main frame");
    const initialFrameUrl = frameTree.frameTree.frame.url;
    this.navigationAllowed = typeof initialFrameUrl === "string" && this.urlRegex.test(initialFrameUrl);
    if (!this.navigationAllowed) cdpError("target_url_mismatch", "the attached main frame URL did not match");
    await Runtime.addBinding({ name: CDP_BINDING_NAME, executionContextName: this.worldName });
    Runtime.bindingCalled((event) => {
      if (event.name !== CDP_BINDING_NAME || event.executionContextId !== this.executionContextId) return;
      try {
        if (Buffer.byteLength(event.payload ?? "", "utf8") > MAX_SIGNAL_BYTES) {
          cdpError("sender_message_too_large", "sender signaling payload exceeded 64 KiB");
        }
        this.bindingRateLimiter.take();
      } catch (error) {
        this.onError(error);
        return;
      }
      Promise.resolve(this.onSignal(event.payload)).catch(this.onError);
    });
    Page.frameNavigated((event) => {
      if (event.frame?.parentId) return;
      const hadActiveSender = this.senderConfig !== null;
      this.mainFrameId = event.frame?.id ?? null;
      this.executionContextId = null;
      this.bindingRateLimiter = new FixedWindowRateLimiter();
      this.navigationAllowed = Boolean(
        this.mainFrameId && typeof event.frame?.url === "string" && this.urlRegex.test(event.frame.url),
      );
      this.resumeAfterNavigation = hadActiveSender && this.navigationAllowed;
      this.identityLostReported = false;
      if (!this.navigationAllowed) {
        this.senderConfig = null;
        const error = new CdpSessionError(
          "target_url_mismatch",
          "the attached target navigated outside its expected URL pattern",
        );
        this.onError(error);
        if (hadActiveSender) this.#reportIdentityLost(error);
      }
    });
    Page.navigatedWithinDocument((event) => {
      if (event.frameId !== this.mainFrameId) return;
      if (typeof event.url !== "string" || !this.urlRegex.test(event.url)) {
        const error = new CdpSessionError(
          "target_url_mismatch",
          "the attached target navigated outside its expected URL pattern",
        );
        const hadActiveSender = this.senderConfig !== null;
        void this.stopSender("target-url-mismatch").finally(() => {
          this.onError(error);
          if (hadActiveSender) this.#reportIdentityLost(error);
        });
        return;
      }
      this.reinstalling = this.inspectPage().catch((error) => {
        this.onError(error);
        this.#reportIdentityLost(error);
      });
    });
    Page.loadEventFired(() => {
      if (!this.navigationAllowed) return;
      const shouldRestart = this.resumeAfterNavigation;
      this.resumeAfterNavigation = false;
      this.reinstalling = this.#installAndResume({ replayJoin: shouldRestart })
        .catch((error) => {
          this.onError(error);
          if (shouldRestart) this.#reportIdentityLost(error);
        });
    });
    const source = this.scriptSource ?? await loadCanvasSenderScript();
    this.scriptSource = source;
    await Page.addScriptToEvaluateOnNewDocument({
      source: this.#classicScriptSource(),
      worldName: this.worldName,
    });
    await this.#installAndResume();
    try {
      return await this.inspectCanvas();
    } catch (error) {
      await this.stopSender("canvas-identity-lost");
      throw error;
    }
  }

  #classicScriptSource() {
    return `${this.scriptSource}\n//# sourceURL=wscrpt-preview-canvas-sender.js`;
  }

  async #installAndResume({ replayJoin = false } = {}) {
    if (!this.client) return;
    const { Runtime } = this.client;
    const world = await this.client.Page.createIsolatedWorld({
      frameId: this.mainFrameId,
      worldName: this.worldName,
      grantUniveralAccess: false,
    });
    this.executionContextId = world.executionContextId;
    if (!Number.isSafeInteger(this.executionContextId)) {
      cdpError("isolated_world_failed", "CDP did not create the preview isolated world");
    }
    const installed = await Runtime.evaluate({
      expression: this.#classicScriptSource(),
      awaitPromise: true,
      returnByValue: true,
      contextId: this.executionContextId,
    });
    if (installed.exceptionDetails) cdpError("sender_injection_failed", "canvas sender injection failed");
    await this.inspectPage();
    if (this.senderConfig) await this.#evaluateStartSender(this.senderConfig);
    if (replayJoin) {
      if (!this.lastJoin) cdpError("navigation_join_missing", "cannot resume navigation without an authenticated join");
      await this.#evaluateReceive(this.lastJoin);
    }
  }

  async inspectPage() {
    if (!this.client) cdpError("not_attached", "CDP session is not attached");
    const locationResult = await this.client.Runtime.evaluate({
      expression: "globalThis.location.href",
      returnByValue: true,
      contextId: this.executionContextId,
    });
    const currentUrl = locationResult.result?.value;
    if (locationResult.exceptionDetails || typeof currentUrl !== "string" || !this.urlRegex.test(currentUrl)) {
      await this.stopSender("target-url-mismatch");
      cdpError("target_url_mismatch", "the attached target navigated outside its expected URL pattern");
    }
    return this.inspectCanvas();
  }

  async inspectCanvas() {
    if (!this.client) cdpError("not_attached", "CDP session is not attached");
    const expression = `(() => {
      try {
        const matches = document.querySelectorAll(${JSON.stringify(this.canvasSelector)});
        if (matches.length !== 1) return { ok: false, count: matches.length, reason: "match-count" };
        const canvas = matches[0];
        if (!(canvas instanceof HTMLCanvasElement)) return { ok: false, count: 1, reason: "not-canvas" };
        if (typeof canvas.captureStream !== "function") return { ok: false, count: 1, reason: "capture-stream-unavailable" };
        return { ok: true, count: 1, width: canvas.width, height: canvas.height };
      } catch (_) {
        return { ok: false, count: 0, reason: "invalid-selector" };
      }
    })()`;
    const result = await this.client.Runtime.evaluate({
      expression,
      returnByValue: true,
      contextId: this.executionContextId,
    });
    if (result.exceptionDetails) cdpError("canvas_inspection_failed", "canvas inspection raised an exception");
    const value = result.result?.value;
    return requireExactCanvasInspection(value);
  }

  async startSender(config) {
    if (!this.client) cdpError("not_attached", "CDP session is not attached");
    try {
      await this.inspectPage();
    } catch (error) {
      this.#reportIdentityLost(error);
      throw error;
    }
    this.senderConfig = structuredClone(config);
    return this.#evaluateStartSender(config);
  }

  async #evaluateStartSender(config) {
    const expression = `(async () => {
      const sender = globalThis.${CDP_SENDER_NAME};
      if (!sender || sender.version !== 1 || typeof sender.start !== "function") throw new Error("sender unavailable");
      return sender.start(${JSON.stringify(config)});
    })()`;
    const result = await this.client.Runtime.evaluate({
      expression,
      awaitPromise: true,
      returnByValue: true,
      contextId: this.executionContextId,
    });
    if (result.exceptionDetails) cdpError("sender_start_failed", "canvas sender failed to start");
    return result.result?.value;
  }

  async receive(message) {
    if (!this.client) cdpError("not_attached", "CDP session is not attached");
    if (message?.type === "join") {
      this.lastJoin = structuredClone(message);
      delete this.lastJoin.token;
      if (this.senderConfig && typeof message.profile === "string") this.senderConfig.profile = message.profile;
    } else if (message?.type === "profile" && typeof message.profile === "string") {
      if (this.senderConfig) this.senderConfig.profile = message.profile;
      if (this.lastJoin) this.lastJoin.profile = message.profile;
    }
    return this.#evaluateReceive(message);
  }

  async #evaluateReceive(message) {
    const expression = `(async () => {
      const receive = globalThis.${CDP_RECEIVE_NAME};
      if (typeof receive !== "function") throw new Error("receiver unavailable");
      return receive(${JSON.stringify(message)});
    })()`;
    const result = await this.client.Runtime.evaluate({
      expression,
      awaitPromise: true,
      returnByValue: true,
      contextId: this.executionContextId,
    });
    if (result.exceptionDetails) cdpError("sender_receive_failed", "canvas sender rejected a signaling message");
    return result.result?.value;
  }

  async snapshotSender() {
    if (!this.client || !Number.isSafeInteger(this.executionContextId)) {
      cdpError("not_attached", "CDP sender isolated world is not attached");
    }
    const expression = `(() => {
      const sender = globalThis.${CDP_SENDER_NAME};
      if (!sender || sender.version !== 1 || typeof sender.snapshot !== "function") return null;
      return sender.snapshot();
    })()`;
    const result = await this.client.Runtime.evaluate({
      expression,
      returnByValue: true,
      contextId: this.executionContextId,
    });
    if (result.exceptionDetails || !result.result?.value) {
      cdpError("sender_snapshot_failed", "canvas sender snapshot is unavailable");
    }
    return result.result.value;
  }

  #reportIdentityLost(error) {
    if (this.identityLostReported || this.closing) return;
    this.identityLostReported = true;
    Promise.resolve(this.onIdentityLost({
      code: "target_identity_lost",
      reason: error?.code === "target_url_mismatch" ? "url-mismatch" : "canvas-identity-lost",
    })).catch(this.onError);
  }

  async stopSender(reason = "preview-stopped") {
    this.senderConfig = null;
    this.lastJoin = null;
    if (!this.client) return;
    const expression = `(async () => {
      const sender = globalThis.${CDP_SENDER_NAME};
      if (sender && typeof sender.stop === "function") return sender.stop({ reason: ${JSON.stringify(reason)} });
    })()`;
    if (!Number.isSafeInteger(this.executionContextId)) return;
    await this.client.Runtime.evaluate({
      expression,
      awaitPromise: true,
      returnByValue: true,
      contextId: this.executionContextId,
    }).catch(this.onError);
  }

  async close() {
    if (!this.client) return;
    this.closing = true;
    await this.stopSender("previewd-detached");
    const client = this.client;
    this.client = null;
    await client.close();
  }
}
