// Reading every page of a listing. Pure: `load` does the I/O.

/** Pages read at most for one listing before giving up. */
export const MAX_LIST_PAGES = 500;
/** Times a listing starts over after its cursor expired. */
const MAX_RESTARTS = 3;

/**
 * Call `load(cursor)` from the first page until a page has no `next_cursor`.
 * A `conflict` on a later page means the core no longer knows the cursor
 * (expired, or cut from an older state): the cursor is dropped and the listing
 * read again from the first page. Callers merge every page they load, which
 * is idempotent, so restarting only re-reads.
 *
 * Resolves `{ok: true, restarts}` or the first failure `{ok: false, error}`.
 */
export async function fetchAllPages(load, { maxPages = MAX_LIST_PAGES, maxRestarts = MAX_RESTARTS } = {}) {
  let cursor = null;
  let restarts = 0;
  for (let pages = 0; pages < maxPages; ) {
    const result = await load(cursor);
    if (!result.ok) {
      if (result.error?.code === "conflict" && cursor !== null && restarts < maxRestarts) {
        restarts += 1;
        cursor = null;
        continue;
      }
      return result;
    }
    pages += 1;
    if (!result.value.next_cursor) return { ok: true, restarts };
    cursor = result.value.next_cursor;
  }
  return {
    ok: false,
    error: { code: "limit_exceeded", message: `一覧が ${maxPages} ページを超えました`, retry: "never" },
  };
}
