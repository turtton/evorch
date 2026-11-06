// Committed history of one thread, newest messages first on screen.
//
// ReadCommittedHistory pages run from the oldest message to the newest and a
// page cursor only moves forward. To show the newest messages and load older
// ones upward, the window walks the pages forward once, keeping only the
// cursor that leads to each page and the items of the last pages. "Load
// older" then asks for the previous page again with its remembered cursor.
// Cursors belong to one frozen cut of the history, so the same cursor returns
// the same slice; if the core forgets a cursor the caller opens a new window.
//
// Messages that arrive as `message_committed` events are appended after the
// walk. A message is identified by `run` and `index`, so the same message
// from a page and an event is shown once.

export const WALK_PAGE_SIZE = 100;
const KEEP_PAGES = 2;
const MAX_WALK_PAGES = 5000;

export function messageKey(message) {
  return `${message.run}#${message.index}`;
}

export class HistoryWindow {
  /**
   * @param {(cursor: string|null) => Promise<{ok: boolean, value?: object, error?: object}>} fetchPage
   *   reads one page of the thread's committed history.
   */
  constructor(fetchPage) {
    this.fetchPage = fetchPage;
    /** One entry per page: the cursor that reads it and, if loaded, its items. */
    this.pages = [];
    /** First page whose items are on screen; every later page is too. */
    this.lo = 0;
    this.live = [];
    this.pending = [];
    this.opened = false;
  }

  /** Walk to the end. Resolves `{ok}` or `{ok: false, error}`. */
  async open() {
    let cursor = null;
    for (let n = 0; n < MAX_WALK_PAGES; n += 1) {
      const result = await this.fetchPage(cursor);
      if (!result.ok) return result;
      const page = result.value;
      this.pages.push({ cursor, items: page.items });
      const old = this.pages.length - 1 - KEEP_PAGES;
      if (old >= 0) this.pages[old].items = null;
      if (!page.next_cursor) break;
      cursor = page.next_cursor;
    }
    // The last KEEP_PAGES pages are on screen, so a short last page does not
    // leave it nearly empty.
    this.lo = Math.max(0, this.pages.length - KEEP_PAGES);
    for (let i = 0; i < this.lo; i += 1) this.pages[i].items = null;
    this.opened = true;
    const buffered = this.pending;
    this.pending = [];
    for (const message of buffered) this.append(message);
    return { ok: true };
  }

  get hasOlder() {
    return this.lo > 0;
  }

  /** Read the page before the oldest one on screen. */
  async loadOlder() {
    if (!this.hasOlder) return { ok: true, added: 0 };
    const index = this.lo - 1;
    const result = await this.fetchPage(this.pages[index].cursor);
    if (!result.ok) return result;
    this.pages[index].items = result.value.items;
    this.lo = index;
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
    for (let i = this.lo; i < this.pages.length; i += 1) {
      if (this.pages[i].items?.some((m) => messageKey(m) === key)) return true;
    }
    return this.live.some((m) => messageKey(m) === key);
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
    for (let i = this.lo; i < this.pages.length; i += 1) for (const m of this.pages[i].items ?? []) take(m);
    for (const m of this.live) take(m);
    return out;
  }
}
