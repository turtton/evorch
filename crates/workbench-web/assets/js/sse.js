// The SSE subscription (GET /api/v1/events) with reconnection.
//
// Message order on one stream: `subscription_started` first, then `ui_event`
// messages. A `resync_required` or `subscription_closed` event is the last
// one. Every message except the adapter's own `resync_required` has an `id`
// of `<epoch>:<sequence>`.
//
// The stream is closed and opened again by this module instead of relying on
// EventSource's built-in retry, so the delay grows exponentially and the
// resume point is explicit: the `after` parameter carries the last cursor
// applied (the same cursor EventSource would send as Last-Event-ID). When
// the core cannot replay from it, it answers with a fresh snapshot.

import { classifyCursor, flattenEvent, formatCursor } from "./wire.js";

export const MAX_BACKOFF_MS = 30_000;
const BASE_BACKOFF_MS = 1_000;
/** After this many failed attempts the next one asks for a fresh snapshot. */
const FAILURES_BEFORE_SNAPSHOT = 3;
/**
 * A stream must stay up this long before the backoff starts over. A core
 * that accepts the subscription and drops it at once keeps the delay growing.
 */
export const STABLE_AFTER_MS = 10_000;

/** Delay before attempt `attempt` (0-based): exponential, capped, half jittered. */
export function backoffDelay(attempt, random = Math.random) {
  const base = Math.min(MAX_BACKOFF_MS, BASE_BACKOFF_MS * 2 ** Math.min(attempt, 16));
  return Math.round(base / 2 + (random() * base) / 2);
}

export function eventsUrl(scope, after) {
  let url = `/api/v1/events?scope=${encodeURIComponent(scope)}`;
  if (after) url += `&after=${encodeURIComponent(formatCursor(after))}`;
  return url;
}

export class EventStream {
  /**
   * @param {object} options
   * @param {string} options.scope `all`, `project:<id>` or `thread:<id>`
   * @param {(started: object) => void} options.onStarted
   * @param {(envelope: object) => void} options.onEvent an event to apply
   * @param {(status: {state: string, retryInMs?: number, reason?: string}) => void} options.onStatus
   * @param {typeof EventSource} [options.EventSourceImpl]
   * @param {(fn: () => void, ms: number, label: string) => unknown} [options.schedule]
   * @param {(handle: unknown) => void} [options.cancel]
   * @param {() => number} [options.random]
   */
  constructor(options) {
    this.scope = options.scope;
    this.onStarted = options.onStarted;
    this.onEvent = options.onEvent;
    this.onStatus = options.onStatus;
    this.EventSourceImpl = options.EventSourceImpl ?? globalThis.EventSource;
    this.schedule = options.schedule ?? ((fn, ms) => setTimeout(fn, ms));
    this.cancel = options.cancel ?? ((handle) => clearTimeout(handle));
    this.random = options.random ?? Math.random;
    this.source = null;
    this.timer = null;
    this.stableTimer = null;
    /** The last cursor applied, or null before the first snapshot. */
    this.last = null;
    this.subscribed = false;
    this.failures = 0;
    this.stopped = true;
  }

  start() {
    this.stopped = false;
    this.connect(this.last);
  }

  stop() {
    this.stopped = true;
    this.close();
    if (this.timer !== null) this.cancel(this.timer);
    this.timer = null;
    this.onStatus({ state: "stopped" });
  }

  /** Reconnect at once (for example when the page becomes visible again). */
  reconnectNow() {
    if (this.stopped || this.subscribed) return;
    if (this.timer !== null) this.cancel(this.timer);
    this.timer = null;
    this.connect(this.failures >= FAILURES_BEFORE_SNAPSHOT ? null : this.last);
  }

  /**
   * Start over with a fresh snapshot (after a backoff delay): the current one
   * can no longer be read to its end, for example because it expired.
   */
  resubscribe(reason = "snapshot_expired") {
    if (this.stopped) return;
    this.last = null;
    this.retry(reason);
  }

  close() {
    this.subscribed = false;
    if (this.stableTimer !== null) this.cancel(this.stableTimer);
    this.stableTimer = null;
    if (this.source) {
      this.source.close();
      this.source = null;
    }
  }

  connect(after) {
    this.close();
    this.timer = null;
    this.onStatus({ state: "connecting" });
    const source = new this.EventSourceImpl(eventsUrl(this.scope, after));
    this.source = source;
    source.addEventListener("subscription_started", (message) => {
      if (this.source !== source) return;
      let started;
      try {
        started = JSON.parse(message.data);
      } catch {
        this.retry("unreadable subscription_started");
        return;
      }
      this.last = started.high_water;
      this.subscribed = true;
      this.stableTimer = this.schedule(
        () => {
          this.stableTimer = null;
          this.failures = 0;
        },
        STABLE_AFTER_MS,
        "stable",
      );
      this.onStatus({ state: "live" });
      this.onStarted(started);
    });
    source.addEventListener("ui_event", (message) => {
      if (this.source !== source || !this.subscribed) return;
      let envelope;
      try {
        envelope = JSON.parse(message.data);
      } catch {
        this.retry("unreadable ui_event");
        return;
      }
      this.handle(envelope);
    });
    source.addEventListener("error", () => {
      if (this.source !== source) return;
      this.retry("disconnected");
    });
  }

  handle(envelope) {
    const event = flattenEvent(envelope.event);
    if (event.type === "resync_required") {
      // Events were dropped (slow subscriber): try to replay from the last
      // applied cursor. An old cursor or a new epoch needs a snapshot.
      // Like any reconnect it waits a backoff delay, so a core that keeps
      // asking for a resync is not hammered.
      const reason = event.payload?.reason;
      if (reason !== "slow_subscriber") this.last = null;
      this.retry(reason ?? "resync_required");
      return;
    }
    if (event.type === "subscription_closed") {
      this.retry(event.payload?.reason ?? "closed");
      return;
    }
    switch (classifyCursor(this.last, envelope.cursor)) {
      case "duplicate":
        return;
      case "epoch_mismatch":
        this.last = null;
        this.retry("epoch_changed");
        return;
      default:
        this.last = envelope.cursor;
        this.onEvent({ ...envelope, event });
    }
  }

  retry(reason) {
    this.close();
    if (this.stopped) return;
    const delay = backoffDelay(this.failures, this.random);
    this.failures += 1;
    this.onStatus({ state: "waiting", retryInMs: delay, reason });
    this.timer = this.schedule(
      () => {
        this.timer = null;
        if (this.stopped) return;
        this.connect(this.failures >= FAILURES_BEFORE_SNAPSHOT ? null : this.last);
      },
      delay,
      "retry",
    );
  }
}
