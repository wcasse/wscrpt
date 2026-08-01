import { isIP } from "node:net";

export const PROTOCOL_VERSION = 1;
export const MAX_SIGNAL_BYTES = 64 * 1024;
export const MAX_ICE_CANDIDATES = 64;
export const MAX_SIGNAL_MESSAGES_PER_WINDOW = 240;
export const SIGNAL_RATE_WINDOW_MS = 10_000;

export const SIGNAL_TYPES = Object.freeze([
  "join",
  "joined",
  "offer",
  "answer",
  "ice",
  "profile",
  "stats",
  "state",
  "error",
  "leave",
]);

export const QUALITY_PROFILE_NAMES = Object.freeze([
  "mini",
  "expanded",
  "expanded-headroom",
  "fallback",
]);

const TYPE_FIELDS = Object.freeze({
  join: new Set(["role", "token", "profile"]),
  joined: new Set(["profile", "source"]),
  offer: new Set(["description", "reason"]),
  answer: new Set(["description"]),
  ice: new Set(["candidate"]),
  profile: new Set(["profile"]),
  stats: new Set(["stats"]),
  state: new Set([
    "state",
    "reason",
    "profile",
    "sourceWidth",
    "sourceHeight",
    "width",
    "height",
    "fps",
  ]),
  error: new Set(["code", "message", "retryable"]),
  leave: new Set(["reason"]),
});

const ENVELOPE_FIELDS = new Set([
  "protocolVersion",
  "sessionId",
  "generation",
  "nonce",
  "type",
]);

const IDENTIFIER = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u;
const NONCE = /^[A-Za-z0-9_-]{16,128}$/u;
const TOKEN = /^[A-Za-z0-9_-]{32,256}$/u;

export class ProtocolError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "ProtocolError";
    this.code = code;
  }
}

function fail(code, message) {
  throw new ProtocolError(code, message);
}

function isPlainObject(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function boundedString(value, field, { min = 0, max = 1024 } = {}) {
  if (typeof value !== "string" || value.length < min || value.length > max) {
    fail("invalid_field", `${field} must be a string between ${min} and ${max} characters`);
  }
  return value;
}

function finiteNumber(value, field, { min = -Infinity, max = Infinity } = {}) {
  if (!Number.isFinite(value) || value < min || value > max) {
    fail("invalid_field", `${field} must be a finite number between ${min} and ${max}`);
  }
  return value;
}

function validateDescription(value, expectedType) {
  if (!isPlainObject(value)) {
    fail("invalid_description", "description must be an object");
  }
  const keys = Object.keys(value);
  if (keys.some((key) => !["type", "sdp"].includes(key))) {
    fail("invalid_description", "description contains an unknown field");
  }
  if (value.type !== expectedType) {
    fail("invalid_description", `description.type must be ${expectedType}`);
  }
  boundedString(value.sdp, "description.sdp", { min: 1, max: 60 * 1024 });
}

function validateCandidate(value) {
  if (value === null) return;
  if (!isPlainObject(value)) fail("invalid_candidate", "candidate must be an object or null");
  const allowed = new Set(["candidate", "sdpMid", "sdpMLineIndex", "usernameFragment"]);
  if (Object.keys(value).some((key) => !allowed.has(key))) {
    fail("invalid_candidate", "candidate contains an unknown field");
  }
  boundedString(value.candidate, "candidate.candidate", { max: 4096 });
  if (value.sdpMid !== null && value.sdpMid !== undefined) {
    boundedString(value.sdpMid, "candidate.sdpMid", { max: 256 });
  }
  if (value.sdpMLineIndex !== null && value.sdpMLineIndex !== undefined) {
    finiteNumber(value.sdpMLineIndex, "candidate.sdpMLineIndex", { min: 0, max: 65535 });
    if (!Number.isInteger(value.sdpMLineIndex)) {
      fail("invalid_candidate", "candidate.sdpMLineIndex must be an integer");
    }
  }
  if (value.usernameFragment !== null && value.usernameFragment !== undefined) {
    boundedString(value.usernameFragment, "candidate.usernameFragment", { max: 256 });
  }
}

function validateJsonTree(value, field, depth = 0) {
  if (depth > 6) fail("invalid_field", `${field} is nested too deeply`);
  if (value === null || typeof value === "boolean" || typeof value === "string") return;
  if (typeof value === "number") {
    finiteNumber(value, field);
    return;
  }
  if (Array.isArray(value)) {
    if (value.length > 128) fail("invalid_field", `${field} has too many entries`);
    value.forEach((entry, index) => validateJsonTree(entry, `${field}[${index}]`, depth + 1));
    return;
  }
  if (!isPlainObject(value)) fail("invalid_field", `${field} must contain JSON values only`);
  const entries = Object.entries(value);
  if (entries.length > 128) fail("invalid_field", `${field} has too many fields`);
  for (const [key, entry] of entries) {
    boundedString(key, `${field} key`, { min: 1, max: 128 });
    validateJsonTree(entry, `${field}.${key}`, depth + 1);
  }
}

function validateTypeFields(message, { allowTokenlessJoin }) {
  switch (message.type) {
    case "join":
      if (message.role !== "receiver") fail("invalid_role", "join.role must be receiver");
      if (message.token === undefined) {
        if (!allowTokenlessJoin) fail("token_required", "initial join requires a token");
      } else if (!TOKEN.test(message.token)) {
        fail("invalid_token", "join.token has an invalid format");
      }
      if (!QUALITY_PROFILE_NAMES.includes(message.profile)) {
        fail("invalid_profile", "join.profile is not supported");
      }
      break;
    case "joined":
      if (!QUALITY_PROFILE_NAMES.includes(message.profile)) {
        fail("invalid_profile", "joined.profile is not supported");
      }
      if (message.source !== undefined) validateJsonTree(message.source, "joined.source");
      break;
    case "offer":
      validateDescription(message.description, "offer");
      if (message.reason !== undefined) boundedString(message.reason, "reason", { max: 512 });
      break;
    case "answer":
      validateDescription(message.description, "answer");
      break;
    case "ice":
      validateCandidate(message.candidate);
      break;
    case "profile":
      if (!QUALITY_PROFILE_NAMES.includes(message.profile)) {
        fail("invalid_profile", "profile is not supported");
      }
      break;
    case "stats":
      if (!isPlainObject(message.stats)) fail("invalid_stats", "stats must be an object");
      validateJsonTree(message.stats, "stats");
      break;
    case "state":
      boundedString(message.state, "state", { min: 1, max: 64 });
      if (message.reason !== undefined) boundedString(message.reason, "reason", { max: 512 });
      if (message.profile !== undefined && !QUALITY_PROFILE_NAMES.includes(message.profile)) {
        fail("invalid_profile", "state.profile is not supported");
      }
      for (const field of ["sourceWidth", "sourceHeight", "width", "height"]) {
        if (message[field] !== undefined) {
          finiteNumber(message[field], field, { min: 1, max: 16384 });
          if (!Number.isInteger(message[field])) fail("invalid_field", `${field} must be an integer`);
        }
      }
      if (message.fps !== undefined) finiteNumber(message.fps, "fps", { min: 1, max: 240 });
      break;
    case "error":
      boundedString(message.code, "code", { min: 1, max: 64 });
      boundedString(message.message, "message", { min: 1, max: 512 });
      if (message.retryable !== undefined && typeof message.retryable !== "boolean") {
        fail("invalid_field", "error.retryable must be a boolean");
      }
      break;
    case "leave":
      if (message.reason !== undefined) boundedString(message.reason, "reason", { max: 512 });
      break;
    default:
      fail("invalid_type", "message type is not supported");
  }
}

function toUtf8String(raw) {
  if (typeof raw === "string") {
    if (Buffer.byteLength(raw, "utf8") > MAX_SIGNAL_BYTES) fail("message_too_large", "message exceeds 64 KiB");
    return raw;
  }
  if (Buffer.isBuffer(raw) || raw instanceof Uint8Array) {
    if (raw.byteLength > MAX_SIGNAL_BYTES) fail("message_too_large", "message exceeds 64 KiB");
    return Buffer.from(raw).toString("utf8");
  }
  if (isPlainObject(raw)) {
    const serialized = JSON.stringify(raw);
    if (Buffer.byteLength(serialized, "utf8") > MAX_SIGNAL_BYTES) fail("message_too_large", "message exceeds 64 KiB");
    return serialized;
  }
  fail("invalid_json", "message must be JSON text or an object");
}

export function validateSignalMessage(raw, options = {}) {
  const {
    expectedSessionId,
    expectedNonce,
    expectedGeneration,
    minimumGeneration,
    allowTokenlessJoin = false,
  } = options;
  let message;
  try {
    message = JSON.parse(toUtf8String(raw));
  } catch (error) {
    if (error instanceof ProtocolError) throw error;
    fail("invalid_json", "message is not valid JSON");
  }
  if (!isPlainObject(message)) fail("invalid_message", "message must be an object");
  if (message.protocolVersion !== PROTOCOL_VERSION) {
    fail("unsupported_version", `protocolVersion must be ${PROTOCOL_VERSION}`);
  }
  if (!IDENTIFIER.test(message.sessionId ?? "")) fail("invalid_session", "sessionId has an invalid format");
  if (!Number.isSafeInteger(message.generation) || message.generation < 1) {
    fail("invalid_generation", "generation must be a positive safe integer");
  }
  if (!NONCE.test(message.nonce ?? "")) fail("invalid_nonce", "nonce has an invalid format");
  if (!SIGNAL_TYPES.includes(message.type)) fail("invalid_type", "message type is not supported");

  const allowed = new Set([...ENVELOPE_FIELDS, ...TYPE_FIELDS[message.type]]);
  if (Object.keys(message).some((key) => !allowed.has(key))) {
    fail("unknown_field", "message contains an unknown field");
  }
  if (expectedSessionId !== undefined && message.sessionId !== expectedSessionId) {
    fail("wrong_session", "message belongs to another session");
  }
  if (expectedNonce !== undefined && message.nonce !== expectedNonce) {
    fail("wrong_nonce", "message nonce does not match this receiver");
  }
  if (expectedGeneration !== undefined && message.generation !== expectedGeneration) {
    fail("stale_generation", "message generation is not current");
  }
  if (minimumGeneration !== undefined && message.generation < minimumGeneration) {
    fail("stale_generation", "message generation is stale");
  }
  validateTypeFields(message, { allowTokenlessJoin });
  return message;
}

export function serializeSignalMessage(message) {
  const validated = validateSignalMessage(message, { allowTokenlessJoin: true });
  const serialized = JSON.stringify(validated);
  if (Buffer.byteLength(serialized, "utf8") > MAX_SIGNAL_BYTES) {
    fail("message_too_large", "message exceeds 64 KiB");
  }
  return serialized;
}

export function isLoopbackHost(host) {
  if (typeof host !== "string" || host.length === 0) return false;
  const normalized = host.startsWith("[") && host.endsWith("]") ? host.slice(1, -1) : host;
  if (normalized === "::1") return true;
  return isIP(normalized) === 4 && normalized.split(".")[0] === "127";
}

export function assertLoopbackHost(host, field = "host") {
  if (!isLoopbackHost(host)) fail("non_loopback", `${field} must be a numeric loopback address`);
  return host.startsWith("[") && host.endsWith("]") ? host.slice(1, -1) : host;
}

export function assertLoopbackUrl(value, { schemes = ["http:", "ws:"], field = "url" } = {}) {
  let url;
  try {
    url = new URL(value);
  } catch {
    fail("invalid_url", `${field} must be an absolute URL`);
  }
  if (!schemes.includes(url.protocol)) fail("invalid_url", `${field} uses an unsupported scheme`);
  assertLoopbackHost(url.hostname, `${field} hostname`);
  if (url.username || url.password) fail("invalid_url", `${field} must not contain credentials`);
  return url;
}

export class FixedWindowRateLimiter {
  constructor({ limit = MAX_SIGNAL_MESSAGES_PER_WINDOW, windowMs = SIGNAL_RATE_WINDOW_MS, now = () => Date.now() } = {}) {
    this.limit = limit;
    this.windowMs = windowMs;
    this.now = now;
    this.startedAt = this.now();
    this.count = 0;
  }

  take() {
    const current = this.now();
    if (current - this.startedAt >= this.windowMs) {
      this.startedAt = current;
      this.count = 0;
    }
    this.count += 1;
    if (this.count > this.limit) fail("rate_limited", "signaling rate limit exceeded");
  }
}

export function redactedSignalMetadata(message) {
  return {
    protocolVersion: message?.protocolVersion,
    sessionId: message?.sessionId,
    generation: message?.generation,
    type: message?.type,
  };
}
