// Committed history of one thread, newest messages first on screen.
//
// ReadCommittedHistory with `direction: "backward"` starts at the tail: the
// first page holds the newest messages (in chronological order) and its
// `next_cursor` addresses the messages before them. Opening reads that one
// page; "load older" follows the cursor upward. The cursors belong to the
// cut the first page was read at; if the core forgets one (`conflict`), the
// caller opens a new window.
//
// Messages that arrive as `message_committed` events are appended after the
// first page. A message is identified by `run` and `index`, so the same
// message from a page and an event is shown once.

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
  }

  /** Read the newest page. Resolves `{ok}` or `{ok: false, error}`. */
  async open() {
    const result = await this.fetchPage(null);
    if (!result.ok) return result;
    this.pages = [result.value.items];
    this.olderCursor = result.value.next_cursor ?? null;
    this.opened = true;
    const buffered = this.pending;
    this.pending = [];
    for (const message of buffered) this.append(message);
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

  /** A committed message from an event. Returns true if it is new. */
  append(message) {
    if (!this.opened) {
      this.pending.push(message);
      return false;
    }
    const key = messageKey(message);
    if (this.has(key)) return false;
    this.live.push(message);
    return true;
  }

  has(key) {
    return (
      this.pages.some((page) => page.some((m) => messageKey(m) === key)) ||
      this.live.some((m) => messageKey(m) === key)
    );
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
