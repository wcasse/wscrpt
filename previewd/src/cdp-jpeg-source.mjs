export class LatestJpegFrameSlot {
  constructor({ consume, acknowledge = async () => {}, onError = () => {} } = {}) {
    if (typeof consume !== "function") throw new TypeError("consume must be a function");
    this.consume = consume;
    this.acknowledge = acknowledge;
    this.onError = onError;
    this.current = null;
    this.pending = null;
    this.draining = false;
    this.closed = false;
    this.accepted = 0;
    this.displayed = 0;
    this.dropped = 0;
    this.acknowledged = 0;
    this.idleWaiters = [];
  }

  offer(frame) {
    if (this.closed) return false;
    this.accepted += 1;
    Promise.resolve()
      .then(() => this.acknowledge(frame.sessionId))
      .then(() => { this.acknowledged += 1; })
      .catch((error) => this.onError(error));

    if (!this.draining) {
      this.current = frame;
      this.draining = true;
      void this.#drain();
    } else {
      if (this.pending !== null) this.dropped += 1;
      this.pending = frame;
    }
    return true;
  }

  async #drain() {
    while (!this.closed && this.current !== null) {
      const frame = this.current;
      try {
        await this.consume(frame);
        this.displayed += 1;
      } catch (error) {
        this.onError(error);
      }
      this.current = this.pending;
      this.pending = null;
    }
    if (this.closed && this.pending !== null) this.dropped += 1;
    this.current = null;
    this.pending = null;
    this.draining = false;
    const waiters = this.idleWaiters.splice(0);
    waiters.forEach((resolve) => resolve());
  }

  waitForIdle() {
    if (!this.draining) return Promise.resolve();
    return new Promise((resolve) => this.idleWaiters.push(resolve));
  }

  close() {
    this.closed = true;
    if (this.pending !== null) {
      this.dropped += 1;
      this.pending = null;
    }
  }

  metrics() {
    return {
      provider: "cdp-jpeg",
      diagnostic: true,
      accepted: this.accepted,
      acknowledged: this.acknowledged,
      displayed: this.displayed,
      dropped: this.dropped,
      queued: this.pending === null ? 0 : 1,
    };
  }
}

export class CdpJpegSource {
  constructor({ Page, consume, onError = () => {}, options = {} }) {
    if (!Page) throw new TypeError("Page domain is required");
    this.Page = Page;
    this.options = {
      format: "jpeg",
      quality: 70,
      maxWidth: 960,
      maxHeight: 540,
      everyNthFrame: 1,
      ...options,
    };
    this.slot = new LatestJpegFrameSlot({
      consume,
      acknowledge: (sessionId) => this.Page.screencastFrameAck({ sessionId }),
      onError,
    });
    this.listener = (event) => this.slot.offer(event);
    this.started = false;
  }

  async start() {
    if (this.started) return;
    this.started = true;
    this.Page.screencastFrame(this.listener);
    await this.Page.startScreencast(this.options);
  }

  async stop() {
    if (!this.started) return;
    this.started = false;
    this.slot.close();
    try {
      await this.Page.stopScreencast();
    } finally {
      if (typeof this.Page.off === "function") this.Page.off("screencastFrame", this.listener);
      else if (typeof this.Page.removeListener === "function") this.Page.removeListener("screencastFrame", this.listener);
    }
  }

  metrics() {
    return this.slot.metrics();
  }
}
