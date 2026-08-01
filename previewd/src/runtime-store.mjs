import { constants as fsConstants } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  open,
  readFile,
  readdir,
  realpath,
  rename,
  rmdir,
  stat,
  unlink,
} from "node:fs/promises";
import { createHash, randomBytes, randomUUID } from "node:crypto";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

const SESSION_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u;
const PRIVATE_DIRECTORY_MODE = 0o700;
const PRIVATE_FILE_MODE = 0o600;
const DEFAULT_TOKEN_TTL_MS = 60_000;
const DEFAULT_HEARTBEAT_TTL_MS = 20_000;
const MAX_EVIDENCE_BYTES = 2 * 1024 * 1024;
const MAX_EVIDENCE_SAMPLES = 3600;
const STALE_LOCK_MS = 30_000;
const EVIDENCE_NUMBER_LIMITS = Object.freeze({
  presentedFps: [0, 240],
  decodedFps: [0, 240],
  frameAgeMs: [0, 60_000],
  presentationAgeMs: [0, 60_000],
  maxFreezeMs: [0, 3_600_000],
  packetLossRatio: [0, 1],
  packetLossDelta: [0, Number.MAX_SAFE_INTEGER],
  packetsReceived: [0, Number.MAX_SAFE_INTEGER],
  packetsReceivedDelta: [0, Number.MAX_SAFE_INTEGER],
  packetsLost: [0, Number.MAX_SAFE_INTEGER],
  framesDecoded: [0, Number.MAX_SAFE_INTEGER],
  framesDropped: [0, Number.MAX_SAFE_INTEGER],
  bytesReceived: [0, Number.MAX_SAFE_INTEGER],
  bitrateBps: [0, 1_000_000_000],
  availableIncomingBitrate: [0, 10_000_000_000],
  jitterSeconds: [0, 60],
  codecPayloadType: [0, 255],
  rttMs: [0, 60_000],
  latencyMs: [0, 60_000],
  width: [1, 16384],
  height: [1, 16384],
});

export class RuntimeStoreError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "RuntimeStoreError";
    this.code = code;
  }
}

function runtimeError(code, message) {
  throw new RuntimeStoreError(code, message);
}

function assertSessionId(sessionId) {
  if (typeof sessionId !== "string" || !SESSION_ID.test(sessionId)) {
    runtimeError("invalid_session", "sessionId has an invalid format");
  }
  return sessionId;
}

function currentUid() {
  return typeof process.getuid === "function" ? process.getuid() : null;
}

function permissions(mode) {
  return mode & 0o777;
}

async function assertPrivateNode(path, expected, kind) {
  const info = await lstat(path);
  if (kind === "directory" && !info.isDirectory()) runtimeError("unsafe_runtime", `${path} is not a directory`);
  if (kind === "file" && !info.isFile()) runtimeError("unsafe_runtime", `${path} is not a regular file`);
  const uid = currentUid();
  if (uid !== null && info.uid !== uid) runtimeError("wrong_owner", `${path} is not owned by the current uid`);
  if (permissions(info.mode) !== expected) runtimeError("unsafe_permissions", `${path} must have mode ${expected.toString(8)}`);
  return info;
}

async function ensurePrivateDirectory(path) {
  await mkdir(path, { mode: PRIVATE_DIRECTORY_MODE, recursive: true });
  const info = await lstat(path);
  const uid = currentUid();
  if (!info.isDirectory()) runtimeError("unsafe_runtime", `${path} is not a directory`);
  if (uid !== null && info.uid !== uid) runtimeError("wrong_owner", `${path} is not owned by the current uid`);
  if (permissions(info.mode) !== PRIVATE_DIRECTORY_MODE) {
    await chmod(path, PRIVATE_DIRECTORY_MODE);
    await assertPrivateNode(path, PRIVATE_DIRECTORY_MODE, "directory");
  }
}

async function ensurePrivateRoot(path) {
  const created = await mkdir(path, { mode: PRIVATE_DIRECTORY_MODE, recursive: true });
  if (created === undefined) {
    await assertPrivateNode(path, PRIVATE_DIRECTORY_MODE, "directory");
    return;
  }
  await chmod(path, PRIVATE_DIRECTORY_MODE);
  await assertPrivateNode(path, PRIVATE_DIRECTORY_MODE, "directory");
}

async function atomicWrite(path, body) {
  const temporary = join(dirname(path), `.${randomUUID()}.tmp`);
  let handle;
  try {
    handle = await open(temporary, fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY, PRIVATE_FILE_MODE);
    await handle.writeFile(body, "utf8");
    await handle.sync();
    await handle.close();
    handle = undefined;
    await rename(temporary, path);
    await chmod(path, PRIVATE_FILE_MODE);
    await assertPrivateNode(path, PRIVATE_FILE_MODE, "file");
  } catch (error) {
    if (handle) await handle.close().catch(() => {});
    await unlink(temporary).catch(() => {});
    throw error;
  }
}

async function readPrivateJson(path) {
  await assertPrivateNode(path, PRIVATE_FILE_MODE, "file");
  let value;
  try {
    value = JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    runtimeError("invalid_runtime_json", `${path} does not contain valid JSON: ${error.message}`);
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    runtimeError("invalid_runtime_json", `${path} must contain a JSON object`);
  }
  return value;
}

function defaultRuntimeRoot() {
  const base = process.env.XDG_RUNTIME_DIR;
  if (base) return join(base, "wscrpt-previewd");
  const uid = currentUid();
  return join(tmpdir(), `wscrpt-previewd-${uid ?? "user"}`);
}

function tokenHash(token) {
  return createHash("sha256").update(token).digest("hex");
}

function isoTime(milliseconds) {
  return new Date(milliseconds).toISOString();
}

export function normalizeEvidenceRecord(record) {
  if (!record || typeof record !== "object" || Array.isArray(record)) {
    runtimeError("invalid_evidence", "evidence record must be an object");
  }
  assertSessionId(record.sessionId);
  if (!Number.isSafeInteger(record.generation) || record.generation < 1) {
    runtimeError("invalid_evidence", "evidence generation must be a positive safe integer");
  }
  let receivedAt;
  try {
    receivedAt = new Date(record.receivedAt).toISOString();
  } catch {
    runtimeError("invalid_evidence", "evidence receivedAt must be an ISO timestamp");
  }
  const metrics = {};
  const input = record.metrics && typeof record.metrics === "object" && !Array.isArray(record.metrics)
    ? record.metrics
    : {};
  for (const [field, [minimum, maximum]] of Object.entries(EVIDENCE_NUMBER_LIMITS)) {
    const value = input[field];
    if (!Number.isFinite(value)) continue;
    if (value < minimum || value > maximum) continue;
    if ([
      "packetLossDelta",
      "packetsReceived",
      "packetsReceivedDelta",
      "packetsLost",
      "framesDecoded",
      "framesDropped",
      "bytesReceived",
      "codecPayloadType",
      "width",
      "height",
    ].includes(field) && !Number.isInteger(value)) continue;
    metrics[field] = value;
  }
  for (const field of ["codec", "frameAgeBasis", "localCandidateType", "remoteCandidateType"]) {
    const value = input[field];
    if (typeof value === "string" && /^[A-Za-z0-9][A-Za-z0-9/._+-]{0,127}$/u.test(value)) metrics[field] = value;
  }
  const profiles = new Set(["mini", "expanded", "expanded-headroom", "fallback"]);
  const profile = profiles.has(record.profile) ? record.profile : "mini";
  return {
    protocolVersion: 1,
    sessionId: record.sessionId,
    generation: record.generation,
    receivedAt,
    profile,
    metrics,
  };
}

function containsForbiddenManifestKey(value, depth = 0) {
  if (depth > 12 || value === null || typeof value !== "object") return false;
  if (Array.isArray(value)) return value.some((entry) => containsForbiddenManifestKey(entry, depth + 1));
  const forbidden = /(token|nonce|sdp|ice|candidate|cookie|authorization|password|secret)/iu;
  return Object.entries(value).some(([key, entry]) => forbidden.test(key) || containsForbiddenManifestKey(entry, depth + 1));
}

export function canonicalEnsureKey({ canonicalRoot, tmuxPane, targetId, canvasSelector }) {
  for (const [name, value] of Object.entries({ canonicalRoot, tmuxPane, targetId, canvasSelector })) {
    if (typeof value !== "string" || value.length === 0) throw new TypeError(`${name} must be a non-empty string`);
  }
  return createHash("sha256")
    .update(JSON.stringify([canonicalRoot, tmuxPane, targetId, canvasSelector]))
    .digest("hex");
}

export function sessionIdForEnsureKey(ensureKey) {
  if (!/^[a-f0-9]{64}$/u.test(ensureKey)) throw new TypeError("ensureKey must be a sha256 hex digest");
  return `p-${ensureKey.slice(0, 32)}`;
}

export class RuntimeStore {
  constructor({
    root = defaultRuntimeRoot(),
    now = () => Date.now(),
    tokenTtlMs = DEFAULT_TOKEN_TTL_MS,
    heartbeatTtlMs = DEFAULT_HEARTBEAT_TTL_MS,
  } = {}) {
    this.root = resolve(root);
    this.sessionsDirectory = join(this.root, "sessions");
    this.configDirectory = join(this.root, "config");
    this.tokensDirectory = join(this.root, "tokens");
    this.evidenceDirectory = join(this.root, "evidence");
    this.locksDirectory = join(this.root, "locks");
    this.now = now;
    this.tokenTtlMs = tokenTtlMs;
    this.heartbeatTtlMs = heartbeatTtlMs;
  }

  async initialize() {
    await ensurePrivateRoot(this.root);
    await Promise.all([
      ensurePrivateDirectory(this.sessionsDirectory),
      ensurePrivateDirectory(this.configDirectory),
      ensurePrivateDirectory(this.tokensDirectory),
      ensurePrivateDirectory(this.evidenceDirectory),
      ensurePrivateDirectory(this.locksDirectory),
    ]);
    return this;
  }

  sessionPath(sessionId) {
    return join(this.sessionsDirectory, `${assertSessionId(sessionId)}.json`);
  }

  configPath(sessionId) {
    return join(this.configDirectory, `${assertSessionId(sessionId)}.json`);
  }

  tokenPath(token) {
    return join(this.tokensDirectory, `${tokenHash(token)}.json`);
  }

  evidencePath(sessionId) {
    return join(this.evidenceDirectory, `${assertSessionId(sessionId)}.jsonl`);
  }

  async writeManifest(manifest) {
    await this.initialize();
    if (!manifest || typeof manifest !== "object" || Array.isArray(manifest)) {
      runtimeError("invalid_manifest", "manifest must be an object");
    }
    assertSessionId(manifest.sessionId);
    if (containsForbiddenManifestKey(manifest)) {
      runtimeError("secret_in_manifest", "normal manifests must not contain signaling credentials or payloads");
    }
    await atomicWrite(this.sessionPath(manifest.sessionId), `${JSON.stringify(manifest, null, 2)}\n`);
    return manifest;
  }

  async readManifest(sessionId, { required = true } = {}) {
    await this.initialize();
    const path = this.sessionPath(sessionId);
    try {
      const manifest = await readPrivateJson(path);
      if (manifest.sessionId !== sessionId) runtimeError("invalid_manifest", "manifest sessionId does not match its file");
      return manifest;
    } catch (error) {
      if (!required && error?.code === "ENOENT") return null;
      throw error;
    }
  }

  async writePrivateConfig(sessionId, config) {
    await this.initialize();
    assertSessionId(sessionId);
    if (!config || typeof config !== "object" || Array.isArray(config)) {
      runtimeError("invalid_config", "private config must be an object");
    }
    await atomicWrite(this.configPath(sessionId), `${JSON.stringify(config, null, 2)}\n`);
  }

  async readPrivateConfig(sessionId) {
    await this.initialize();
    return readPrivateJson(this.configPath(sessionId));
  }

  async listManifests({ canonicalRoot } = {}) {
    await this.initialize();
    const names = await readdir(this.sessionsDirectory);
    const manifests = [];
    for (const name of names.sort()) {
      if (!name.endsWith(".json")) continue;
      try {
        const manifest = await readPrivateJson(join(this.sessionsDirectory, name));
        if (canonicalRoot === undefined || manifest.workspace?.canonicalRoot === canonicalRoot) manifests.push(manifest);
      } catch (error) {
        if (error?.code !== "ENOENT") throw error;
      }
    }
    return manifests;
  }

  async updateManifest(sessionId, mutate) {
    return this.withSessionLock(sessionId, async () => {
      const current = await this.readManifest(sessionId);
      const updated = await mutate(structuredClone(current));
      if (!updated || updated.sessionId !== sessionId) runtimeError("invalid_manifest", "updated manifest changed its identity");
      await this.writeManifest(updated);
      return updated;
    });
  }

  async heartbeat(sessionId, patch = {}) {
    return this.updateManifest(sessionId, (manifest) => ({
      ...manifest,
      ...patch,
      heartbeatAt: isoTime(this.now()),
    }));
  }

  isHeartbeatFresh(manifest, ttlMs = this.heartbeatTtlMs) {
    const heartbeat = Date.parse(manifest?.heartbeatAt ?? "");
    return Number.isFinite(heartbeat) && this.now() - heartbeat <= ttlMs;
  }

  async issueToken({ sessionId, generation, ttlMs = this.tokenTtlMs }) {
    await this.initialize();
    assertSessionId(sessionId);
    if (!Number.isSafeInteger(generation) || generation < 1) {
      runtimeError("invalid_generation", "generation must be a positive safe integer");
    }
    if (!Number.isFinite(ttlMs) || ttlMs < 1 || ttlMs > 5 * 60_000) {
      runtimeError("invalid_ttl", "token ttl must be between 1 ms and 5 minutes");
    }
    const token = randomBytes(32).toString("base64url");
    const nonce = randomBytes(18).toString("base64url");
    const issuedAt = this.now();
    const record = {
      sessionId,
      generation,
      nonce,
      issuedAt: isoTime(issuedAt),
      expiresAt: isoTime(issuedAt + ttlMs),
    };
    await atomicWrite(this.tokenPath(token), `${JSON.stringify(record)}\n`);
    return { token, nonce, ...record };
  }

  async consumeToken({ token, sessionId, generation, nonce }) {
    await this.initialize();
    if (typeof token !== "string" || token.length < 32 || token.length > 256) {
      runtimeError("invalid_token", "token has an invalid format");
    }
    assertSessionId(sessionId);
    const path = this.tokenPath(token);
    const claimedPath = `${path}.${process.pid}.${randomUUID()}.claimed`;
    try {
      await rename(path, claimedPath);
    } catch (error) {
      if (error?.code === "ENOENT") runtimeError("token_unavailable", "token is invalid, expired, or already used");
      throw error;
    }
    let record;
    try {
      record = await readPrivateJson(claimedPath);
    } finally {
      await unlink(claimedPath).catch(() => {});
    }
    if (Date.parse(record.expiresAt) <= this.now()) runtimeError("token_expired", "token has expired");
    if (record.sessionId !== sessionId || record.generation !== generation || record.nonce !== nonce) {
      runtimeError("token_scope", "token does not belong to this session generation and nonce");
    }
    return record;
  }

  async revokeSessionTokens(sessionId) {
    await this.initialize();
    assertSessionId(sessionId);
    let removed = 0;
    for (const name of await readdir(this.tokensDirectory)) {
      if (!name.endsWith(".json")) continue;
      const path = join(this.tokensDirectory, name);
      try {
        const record = await readPrivateJson(path);
        if (record.sessionId === sessionId) {
          await unlink(path);
          removed += 1;
        }
      } catch (error) {
        if (error?.code !== "ENOENT") throw error;
      }
    }
    return removed;
  }

  async appendEvidence(record) {
    await this.initialize();
    const normalized = normalizeEvidenceRecord(record);
    const path = this.evidencePath(normalized.sessionId);
    let lines = [];
    try {
      await assertPrivateNode(path, PRIVATE_FILE_MODE, "file");
      const existing = await readFile(path, "utf8");
      if (Buffer.byteLength(existing, "utf8") > MAX_EVIDENCE_BYTES) {
        runtimeError("evidence_too_large", "evidence file exceeded its configured cap");
      }
      lines = existing.split("\n").filter(Boolean);
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
    const encoded = JSON.stringify(normalized);
    if (Buffer.byteLength(encoded, "utf8") > 8192) runtimeError("evidence_sample_too_large", "evidence sample exceeded 8 KiB");
    lines.push(encoded);
    while (lines.length > MAX_EVIDENCE_SAMPLES) lines.shift();
    while (lines.length > 1 && Buffer.byteLength(`${lines.join("\n")}\n`, "utf8") > MAX_EVIDENCE_BYTES) lines.shift();
    await atomicWrite(path, `${lines.join("\n")}\n`);
    return normalized;
  }

  async readEvidence(sessionId, { tail = MAX_EVIDENCE_SAMPLES } = {}) {
    await this.initialize();
    assertSessionId(sessionId);
    if (!Number.isSafeInteger(tail) || tail < 0 || tail > MAX_EVIDENCE_SAMPLES) {
      runtimeError("invalid_evidence_tail", `evidence tail must be between 0 and ${MAX_EVIDENCE_SAMPLES}`);
    }
    const path = this.evidencePath(sessionId);
    let records = [];
    try {
      await assertPrivateNode(path, PRIVATE_FILE_MODE, "file");
      const body = await readFile(path, "utf8");
      if (Buffer.byteLength(body, "utf8") > MAX_EVIDENCE_BYTES) {
        runtimeError("evidence_too_large", "evidence file exceeded its configured cap");
      }
      records = body.split("\n").filter(Boolean).map((line) => normalizeEvidenceRecord(JSON.parse(line)));
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
    const selected = tail === 0 ? [] : records.slice(-tail);
    return {
      path,
      sampleCount: records.length,
      firstReceivedAt: records[0]?.receivedAt ?? null,
      lastReceivedAt: records.at(-1)?.receivedAt ?? null,
      samples: selected,
    };
  }

  async cleanupExpired({ heartbeatTtlMs = this.heartbeatTtlMs, removeConfigs = false } = {}) {
    await this.initialize();
    const now = this.now();
    let expiredTokens = 0;
    for (const name of await readdir(this.tokensDirectory)) {
      if (!name.endsWith(".json")) continue;
      const path = join(this.tokensDirectory, name);
      try {
        const record = await readPrivateJson(path);
        if (!Number.isFinite(Date.parse(record.expiresAt)) || Date.parse(record.expiresAt) <= now) {
          await unlink(path);
          expiredTokens += 1;
        }
      } catch (error) {
        if (error?.code !== "ENOENT") throw error;
      }
    }

    const staleSessions = [];
    for (const manifest of await this.listManifests()) {
      const heartbeat = Date.parse(manifest.heartbeatAt ?? "");
      if (manifest.state !== "stopped" && (!Number.isFinite(heartbeat) || now - heartbeat > heartbeatTtlMs)) {
        staleSessions.push(manifest.sessionId);
        await this.writeManifest({ ...manifest, state: "stale" });
        await this.revokeSessionTokens(manifest.sessionId);
        if (removeConfigs) await unlink(this.configPath(manifest.sessionId)).catch(() => {});
      }
    }
    return { expiredTokens, staleSessions };
  }

  async removeSession(sessionId) {
    await this.initialize();
    assertSessionId(sessionId);
    await this.revokeSessionTokens(sessionId);
    await Promise.all([
      unlink(this.sessionPath(sessionId)).catch((error) => { if (error?.code !== "ENOENT") throw error; }),
      unlink(this.configPath(sessionId)).catch((error) => { if (error?.code !== "ENOENT") throw error; }),
      unlink(this.evidencePath(sessionId)).catch((error) => { if (error?.code !== "ENOENT") throw error; }),
    ]);
  }

  async withSessionLock(sessionId, operation, { timeoutMs = 2_000 } = {}) {
    await this.initialize();
    const lock = join(this.locksDirectory, `${assertSessionId(sessionId)}.lock`);
    const deadline = Date.now() + timeoutMs;
    while (true) {
      try {
        await mkdir(lock, { mode: PRIVATE_DIRECTORY_MODE });
        break;
      } catch (error) {
        if (error?.code !== "EEXIST") throw error;
        let info;
        try {
          info = await assertPrivateNode(lock, PRIVATE_DIRECTORY_MODE, "directory");
        } catch (lockError) {
          if (lockError?.code === "ENOENT") continue;
          throw lockError;
        }
        if (Date.now() - info.mtimeMs > STALE_LOCK_MS) {
          try {
            await rmdir(lock);
            continue;
          } catch (lockError) {
            if (["ENOENT", "ENOTEMPTY"].includes(lockError?.code)) continue;
            throw lockError;
          }
        }
        if (Date.now() >= deadline) runtimeError("lock_timeout", `timed out locking session ${sessionId}`);
        await new Promise((resolvePromise) => setTimeout(resolvePromise, 10));
      }
    }
    try {
      return await operation();
    } finally {
      await rmdir(lock);
    }
  }

  async canonicalWorkspace(path) {
    const resolved = await realpath(path);
    const info = await stat(resolved);
    if (!info.isDirectory()) runtimeError("invalid_workspace", "workspace must be a directory");
    return resolved;
  }
}

export const runtimeStoreDefaults = Object.freeze({
  directoryMode: PRIVATE_DIRECTORY_MODE,
  fileMode: PRIVATE_FILE_MODE,
  tokenTtlMs: DEFAULT_TOKEN_TTL_MS,
  heartbeatTtlMs: DEFAULT_HEARTBEAT_TTL_MS,
  maxEvidenceBytes: MAX_EVIDENCE_BYTES,
  maxEvidenceSamples: MAX_EVIDENCE_SAMPLES,
  staleLockMs: STALE_LOCK_MS,
});
