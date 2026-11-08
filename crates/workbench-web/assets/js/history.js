// Committed history of one thread, newest messages first on screen.
//
// ReadCommittedHistory with `direction: "backward"` starts at the tail: the
// first page holds the newest messages (in chronological order) and its
// `next_cursor` addresses the messages before them. Opening reads that one
// page; "load older" follows the cursor upward. The cursors belong to the
// cut the first page was read at; if the core forgets one (`conflict`), the
// caller opens a new window.
//
// Messages that arrive as `message_committed` events (live or replayed after
// a reconnect) are placed by their position, not by arrival: `index` counts
// the messages of one run. The newest page holds every run's latest messages
// at the cut it was read at, so an event at or below its run's highest index
// is already on screen or older than the page and is dropped; only a higher
// index is new and goes after the page.
//
// A run the page does not show may be older than the page (all its messages
// precede the newest page) or newer (started after the cut); its index says
// nothing about which, even 0. Events arrive in cursor order, so once one of
// them is provably after the cut (a message above its run's boundary), every
// later event is too: from then on, a message of a run the page does not
// show is new. Until then such a message cannot be placed and the caller
// re-reads the history. An empty history has no cut to be behind.

import { compareDecimal } from "./wire.js";

export const HISTORY_PAGE_SIZE = 50;

export function messageKey(message) {
  return `${message.run}#${message.index}`;
}

export class HistoryWindow {
  /**
   * @param {(cursor: string|null) => Promise<{ok: boolean, value?: object, error?: object}>} fetchPage
   *   reads one backward page of the thread's committed history.
   */
  constructor(fetchPage) {
    this.fetchPage = fetchPage;
    /** Loaded pages, oldest first; each is chronological. */
    this.pages = [];
    /** Cursor of the next older page, or null at the start of the history. */
    this.olderCursor = null;
    this.live = [];
    this.pending = [];
    this.opened = false;
    /** Highest index on screen per run: the boundary new messages must pass. */
    this.latest = new Map();
    /** An event already proved the stream to be past the newest page's cut. */
    this.pastCut = false;
  }

  note(message) {
    const held = this.latest.get(message.run);
    if (held === undefined || compareDecimal(message.index, held) > 0) {
      this.latest.set(message.run, message.index);
    }
  }

  /** Read the newest page. Resolves `{ok}` or `{ok: false, error}`. */
  async open() {
    const result = await this.fetchPage(null);
    if (!result.ok) return result;
    this.pages = [result.value.items];
    this.olderCursor = result.value.next_cursor ?? null;
    for (const message of result.value.items) this.note(message);
    this.pastCut = result.value.items.length === 0 && this.olderCursor === null;
    this.opened = true;
    const buffered = this.pending;
    this.pending = [];
    for (const message of buffered) {
      if (this.append(message) === "resync") return { ok: true, resync: true };
    }
    return { ok: true };
  }

  get hasOlder() {
    return this.olderCursor !== null;
  }

  /** Read the page before the oldest one on screen. */
  async loadOlder() {
    if (!this.hasOlder) return { ok: true, added: 0 };
    const result = await this.fetchPage(this.olderCursor);
    if (!result.ok) return result;
    this.pages.unshift(result.value.items);
    this.olderCursor = result.value.next_cursor ?? null;
    return { ok: true, added: result.value.items.length };
  }

  /**
   * A committed message from an event. Returns "added" when it is newer
   * than everything on screen for its run, "ignored" when it is on screen
   * or older than the page, and "resync" when its place cannot be known.
   * Before the first page arrives events are held and placed afterwards.
   */
  append(message) {
    if (!this.opened) {
      this.pending.push(message);
      return "held";
    }
    const held = this.latest.get(message.run);
    if (held === undefined) {
      if (!this.pastCut) return "resync";
    } else if (compareDecimal(message.index, held) <= 0) {
      return "ignored";
    } else {
      this.pastCut = true;
    }
    this.live.push(message);
    this.note(message);
    return "added";
  }

  /** Everything on screen, oldest first, without duplicates. */
  messages() {
    const seen = new Set();
    const out = [];
    const take = (m) => {
      const key = messageKey(m);
      if (seen.has(key)) return;
      seen.add(key);
      out.push(m);
    };
    for (const page of this.pages) for (const m of page) take(m);
    for (const m of this.live) take(m);
    return out;
  }
}
