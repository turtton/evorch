// When to re-read the newest history page after events without a place.
//
// A burst of such events causes one read after a short quiet time. Reads
// are budgeted: at most `max` start within any `windowMs`. Over budget the
// scheduler does not drop the request; it reports a pause and starts the
// read as soon as the oldest read in the window leaves it, so whatever is
// still unshown is read in one go.

export class RereadScheduler {
  /**
   * @param {object} options
   * @param {() => void} options.run starts one re-read
   * @param {(waitMs: number) => void} [options.onPaused] over budget; the read waits this long
   * @param {number} options.debounceMs
   * @param {number} options.max
   * @param {number} options.windowMs
   * @param {() => number} [options.now]
   * @param {(fn: () => void, ms: number) => unknown} [options.setTimer]
   * @param {(handle: unknown) => void} [options.clearTimer]
   */
  constructor({ run, onPaused = () => {}, debounceMs, max, windowMs, now = Date.now, setTimer, clearTimer }) {
    this.runRead = run;
    this.onPaused = onPaused;
    this.debounceMs = debounceMs;
    this.max = max;
    this.windowMs = windowMs;
    this.now = now;
    this.setTimer = setTimer ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = clearTimer ?? ((handle) => clearTimeout(handle));
    /** Start times of the reads in the current window. */
    this.started = [];
    this.timer = null;
    this.paused = false;
    this.disposed = false;
  }

  /** An event had no place: read soon, once for the whole burst. */
  request() {
    if (this.disposed || this.timer !== null) return;
    this.timer = this.setTimer(() => this.fire(), this.debounceMs);
  }

  fire() {
    this.timer = null;
    if (this.disposed) return;
    const now = this.now();
    this.started = this.started.filter((at) => now - at < this.windowMs);
    if (this.started.length >= this.max) {
      const wait = Math.max(1, this.started[0] + this.windowMs - now);
      this.paused = true;
      this.onPaused(wait);
      this.timer = this.setTimer(() => this.fire(), wait);
      return;
    }
    this.paused = false;
    this.started.push(now);
    this.runRead();
  }

  /** The person read the history by hand: forget the budget and any wait. */
  reset() {
    if (this.timer !== null) this.clearTimer(this.timer);
    this.timer = null;
    this.started = [];
    this.paused = false;
  }

  dispose() {
    this.reset();
    this.disposed = true;
  }
}
