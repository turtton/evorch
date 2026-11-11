// Entry point: wires the API, the read model, the event stream and the views.

import { Api } from "./api.js";
import { replaceChildren, h } from "./dom.js";
import { coreChanged, protocolMismatch } from "./format.js";
import { EventStream } from "./sse.js";
import { createStore } from "./store.js";
import { homeView, inboxView, projectView, threadView } from "./views.js";
import { fetchAllPages } from "./paging.js";
import { describeError, pageRequest } from "./wire.js";

const DEFAULT_BODY_CHUNK = 64 * 1024;
const TOAST_MS = 3000;

const els = {
  back: document.getElementById("back"),
  title: document.getElementById("title"),
  conn: document.getElementById("conn"),
  connLabel: document.getElementById("conn-label"),
  inbox: document.getElementById("inbox-link"),
  inboxBadge: document.getElementById("inbox-badge"),
  banner: document.getElementById("banner"),
  view: document.getElementById("view"),
  toast: document.getElementById("toast"),
};

const api = new Api();
const store = createStore();
/** The ServerHello the page started with, and the latest one. */
let firstHello = null;
let hello = null;
let bannerKind = null;
let current = null;
let toastTimer = null;

function setTitle(text) {
  els.title.textContent = text;
  document.title = text === "evorch" ? "evorch" : `${text} - evorch`;
}

function setBack(hash) {
  els.back.hidden = !hash;
  els.back.dataset.target = hash ?? "";
}

function toast(text) {
  els.toast.textContent = text;
  els.toast.hidden = false;
  if (toastTimer !== null) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    els.toast.hidden = true;
    els.toast.textContent = "";
  }, TOAST_MS);
}

/** Show a banner; `kind` lets a later state clear only its own banner. */
function showBanner(kind, text, action) {
  bannerKind = kind;
  replaceChildren(
    els.banner,
    h("span", { class: "banner-text", text }),
    action ? h("button", { type: "button", class: "button small", text: action.label, on: { click: action.run } }) : null,
  );
  els.banner.hidden = false;
}

function clearBanner(kind) {
  if (bannerKind !== kind) return;
  bannerKind = null;
  els.banner.hidden = true;
  replaceChildren(els.banner);
}

const ctx = {
  api,
  store,
  toast,
  setTitle,
  setBack,
  threadTitle(threadId) {
    return store.state.threads.get(threadId)?.title || "スレッドを開く";
  },
  maxBodyChunk() {
    return hello?.limits?.max_body_chunk_bytes ?? DEFAULT_BODY_CHUNK;
  },
  /**
   * Run a listing query and merge its page into the store as of the moment
   * it was sent, so it cannot overwrite what a later event wrote.
   */
  async queryInto(kind, type, payload) {
    const issued = store.mark();
    const result = await api.query(type, payload);
    if (result.ok) store.mergePage(kind, result.value.items, issued);
    return result;
  },
  /** Re-read one thread's questions, answered ones included, after acting on one. */
  refreshQuestions(threadId) {
    return fetchAllPages((cursor) =>
      ctx.queryInto("questions", "list_questions", {
        thread: threadId,
        pending_only: false,
        page: pageRequest(cursor, 200),
      }),
    );
  },
  refreshApprovals(threadId) {
    return fetchAllPages((cursor) =>
      ctx.queryInto("approvals", "list_approvals", {
        thread: threadId,
        pending_only: false,
        page: pageRequest(cursor, 200),
      }),
    );
  },
};

/**
 * A snapshot whose first page was cut short is read to its end with
 * ReadSnapshot. The later pages are the same cut, applied under the snapshot's
 * stamp so they never overwrite what an event has written since. If the core
 * no longer holds the snapshot (expired, or a new epoch), the subscription
 * starts over for a fresh one. Another failure leaves "partial" on screen with
 * a retry that continues from the page that failed. A newer snapshot cancels
 * this reading.
 */
let continuation = 0;
async function continueSnapshot(snapshotId, stamp, cursor) {
  const mine = (continuation += 1);
  clearBanner("partial");
  let next = cursor;
  while (next) {
    const result = await api.query("read_snapshot", {
      snapshot_id: snapshotId,
      page: pageRequest(next, 200),
    });
    if (mine !== continuation || store.snapshotMark() !== stamp) return;
    if (!result.ok || result.value.snapshot_id !== snapshotId) {
      const code = result.ok ? "resync_required" : result.error.code;
      if (code === "resync_required" || code === "conflict" || code === "not_found") {
        stream.resubscribe("snapshot_expired");
        return;
      }
      showBanner("partial", `一覧の一部を読み込めませんでした（${describeError(result.error)}）。`, {
        label: "再試行",
        run: () => continueSnapshot(snapshotId, stamp, next),
      });
      return;
    }
    store.applySnapshotPage(result.value.page.items, stamp);
    next = result.value.page.next_cursor ?? null;
  }
  store.completeSnapshot();
}

// ---- navigation -----------------------------------------------------------

function parseRoute(hash) {
  const path = hash.replace(/^#/, "") || "/";
  const parts = path.split("/").filter(Boolean).map(decodeURIComponent);
  if (parts[0] === "p" && parts[1]) return { name: "project", id: parts[1] };
  if (parts[0] === "t" && parts[1]) return { name: "thread", id: parts[1] };
  if (parts[0] === "inbox") return { name: "inbox" };
  return { name: "home" };
}

function navigate() {
  current?.destroy();
  const route = parseRoute(location.hash);
  switch (route.name) {
    case "project":
      current = projectView(ctx, route.id);
      break;
    case "thread":
      current = threadView(ctx, route.id);
      break;
    case "inbox":
      current = inboxView(ctx);
      break;
    default:
      current = homeView(ctx);
  }
  replaceChildren(els.view, current.el);
  els.view.dataset.route = route.name;
  setTitle(current.title);
  setBack(current.back);
  els.inbox.classList.toggle("active", route.name === "inbox");
  window.scrollTo(0, 0);
}

els.back.addEventListener("click", () => {
  const target = els.back.dataset.target;
  if (target) location.hash = target;
});
window.addEventListener("hashchange", navigate);

// ---- attention badge ------------------------------------------------------

function renderBadge() {
  const total = store.totalAttention();
  const count = total.questions + total.approvals;
  const partial = store.state.partial;
  els.inboxBadge.textContent = partial ? `${count}+` : String(count);
  els.inboxBadge.hidden = count === 0 && !partial;
  els.inbox.setAttribute(
    "aria-label",
    (count === 0 ? "要対応（なし）" : `要対応: 質問 ${total.questions} 件、承認 ${total.approvals} 件`) +
      (partial ? "（一部のみ表示・読み込み中）" : ""),
  );
}

store.subscribe((change) => {
  renderBadge();
  current?.onChange(change);
});

// ---- core health ------------------------------------------------------------

async function checkHealth() {
  const result = await api.health();
  if (!result.ok) {
    if (bannerKind !== "restarted") {
      showBanner("unreachable", `コアに接続できません（${describeError(result.error)}）。再接続を待っています。`);
    }
    return;
  }
  hello = result.value.hello;
  clearBanner("unreachable");
  if (!firstHello) {
    firstHello = hello;
    const mismatch = protocolMismatch(hello);
    if (mismatch) {
      showBanner("incompatible", mismatch, {
        label: "再読み込み",
        run: () => location.reload(),
      });
    }
    return;
  }
  if (coreChanged(firstHello, hello)) {
    showBanner("restarted", "コアが再起動しました。最新の画面にするにはページを再読み込みしてください。", {
      label: "再読み込み",
      run: () => location.reload(),
    });
  }
}

// ---- event stream -----------------------------------------------------------

const CONNECTION_LABELS = {
  connecting: "接続中…",
  live: "接続済み",
  waiting: "再接続待ち",
  stopped: "切断",
};

const stream = new EventStream({
  scope: "all",
  onStarted(started) {
    if (store.applySubscriptionStarted(started) && store.state.partial) {
      continueSnapshot(started.snapshot_id, store.snapshotMark(), started.initial_page.next_cursor);
    }
  },
  onEvent(envelope) {
    store.applyEvent(envelope.event);
  },
  onStatus(status) {
    els.conn.dataset.state = status.state;
    let text = CONNECTION_LABELS[status.state] ?? status.state;
    if (status.state === "waiting" && status.retryInMs !== undefined) {
      text = `${text}（${Math.ceil(status.retryInMs / 1000)} 秒後）`;
    }
    els.connLabel.textContent = text;
    if (status.state === "live" || status.state === "waiting") checkHealth();
  },
});

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState !== "visible") return;
  stream.reconnectNow();
  checkHealth();
});

renderBadge();
navigate();
checkHealth();
stream.start();
