import { PREVIEW_PROFILES, validateReceiverSession } from "./webrtc-provider.mjs";

function defaultDecodeFrame(frame) {
  let blob;
  if (frame.data instanceof Blob) {
    blob = frame.data;
  } else if (frame.data instanceof ArrayBuffer || ArrayBuffer.isView(frame.data)) {
    blob = new Blob([frame.data], { type: frame.mimeType ?? "image/jpeg" });
  } else if (typeof frame.data === "string") {
    const bytes = Uint8Array.from(atob(frame.data), (character) => character.charCodeAt(0));
    blob = new Blob([bytes], { type: frame.mimeType ?? "image/jpeg" });
  } else {
    throw new Error("unsupported diagnostic JPEG payload");
  }
  if (typeof globalThis.createImageBitmap !== "function") {
    throw new Error("createImageBitmap is required for diagnostic JPEG decoding");
  }
  return globalThis.createImageBitmap(blob);
}

/**
 * A capacity-one decoder. A decoded frame is discarded if a newer encoded
 * frame arrived while it was decoding, so stale images never drain later.
 */
export class LatestJpegDecoder {
  constructor({ decodeFrame = defaultDecodeFrame, renderFrame, onError = () => {} } = {}) {
    if (typeof renderFrame !== "function") throw new Error("renderFrame is required");
    this.decodeFrame = decodeFrame;
    this.renderFrame = renderFrame;
    this.onError = onError;
    this.latest = null;
    this.decoding = false;
    this.closed = false;
    this.offered = 0;
    this.rendered = 0;
    this.droppedSuperseded = 0;
    this.lastSequence = -1;
  }

  offer(frame) {
    if (this.closed || !frame || typeof frame !== "object") return false;
    const sequence = Number(frame.sequence);
    if (!Number.isSafeInteger(sequence) || sequence <= this.lastSequence) return false;
    this.lastSequence = sequence;
    this.offered += 1;
    if (this.latest) this.droppedSuperseded += 1;
    this.latest = { ...frame, sequence };
    void this.pump();
    return true;
  }

  async pump() {
    if (this.decoding || this.closed) return;
    this.decoding = true;
    try {
      while (!this.closed && this.latest) {
        const frame = this.latest;
        this.latest = null;
        let decoded;
        try {
          decoded = await this.decodeFrame(frame);
        } catch (error) {
          this.onError(error);
          continue;
        }
        if (this.closed || this.latest?.sequence > frame.sequence) {
          this.droppedSuperseded += 1;
          decoded?.close?.();
          continue;
        }
        try {
          await this.renderFrame(decoded, frame);
          this.rendered += 1;
        } catch (error) {
          this.onError(error);
        } finally {
          decoded?.close?.();
        }
      }
    } finally {
      this.decoding = false;
      if (!this.closed && this.latest) void this.pump();
    }
  }

  stats() {
    return {
      diagnostic: true,
      offeredFrames: this.offered,
      renderedFrames: this.rendered,
      droppedSuperseded: this.droppedSuperseded,
      pendingFrames: this.latest ? 1 : 0,
    };
  }

  close() {
    this.closed = true;
    this.latest = null;
  }
}

class JpegConnection {
  constructor(provider, session, signal) {
    this.provider = provider;
    this.session = session;
    this.signal = signal;
    this.closed = false;
    this.sourceHandle = null;
    this.canvas = provider.documentRef.createElement("canvas");
    this.canvas.width = PREVIEW_PROFILES.fallback.width;
    this.canvas.height = PREVIEW_PROFILES.fallback.height;
    this.context = this.canvas.getContext("2d", { alpha: false });
    if (!this.context || typeof this.canvas.captureStream !== "function") {
      throw new Error("diagnostic JPEG canvas capture is unavailable");
    }
    this.stream = this.canvas.captureStream(PREVIEW_PROFILES.fallback.fps);
    this.decoder = new LatestJpegDecoder({
      decodeFrame: provider.decodeFrame,
      renderFrame: (image) => {
        this.context.drawImage(image, 0, 0, this.canvas.width, this.canvas.height);
      },
      onError: provider.onError,
    });
  }

  async start() {
    if (this.signal?.aborted) throw this.signal.reason ?? new Error("preview attach aborted");
    if (!this.provider.frameSource || typeof this.provider.frameSource.connect !== "function") {
      throw new Error("diagnostic JPEG source is not configured");
    }
    if (this.signal) {
      this.abort = () => this.close();
      this.signal.addEventListener("abort", this.abort, { once: true });
    }
    const sourceHandle = await this.provider.frameSource.connect({
      session: this.session,
      signal: this.signal,
      onFrame: (frame) => this.decoder.offer(frame),
    });
    if (this.closed || this.signal?.aborted) {
      sourceHandle?.close?.();
      throw this.signal?.reason ?? new Error("preview attach aborted");
    }
    this.sourceHandle = sourceHandle;
    return Object.freeze({
      stream: this.stream,
      diagnostic: true,
      setProfile: async (profile) => {
        if (profile !== "fallback") {
          throw new Error("diagnostic JPEG provider is fixed at the 12 FPS fallback profile");
        }
      },
      sampleStats: async () => ({
        ...this.decoder.stats(),
        profile: "fallback",
        generation: this.session.generation,
        width: this.canvas.width,
        height: this.canvas.height,
      }),
      restart: async () => this.sourceHandle?.restart?.(),
      close: () => this.close(),
      onState: () => () => {},
      getGeneration: () => this.session.generation,
    });
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    this.decoder.close();
    this.sourceHandle?.close?.();
    this.sourceHandle = null;
    if (this.signal && this.abort) this.signal.removeEventListener("abort", this.abort);
    for (const track of this.stream.getTracks()) track.stop?.();
  }
}

/**
 * Clearly diagnostic provider. The CDP source is injected by previewd so this
 * module does not invent a second signaling protocol or put JPEGs in /signal.
 */
export class JpegPreviewProvider {
  constructor({
    frameSource = null,
    documentRef = globalThis.document,
    decodeFrame = defaultDecodeFrame,
    onError = () => {},
  } = {}) {
    if (!documentRef) throw new Error("a document is required for diagnostic JPEG rendering");
    this.frameSource = frameSource;
    this.documentRef = documentRef;
    this.decodeFrame = decodeFrame;
    this.onError = onError;
  }

  async connect({ session, profile = "fallback", signal } = {}) {
    validateReceiverSession(session);
    if (profile !== "fallback") {
      throw new Error("diagnostic JPEG provider only supports the fallback profile");
    }
    return new JpegConnection(this, session, signal).start();
  }
}
