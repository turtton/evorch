// Screens: project list, thread list, thread, and the "needs attention" inbox.
//
// A view is `{el, title, back, onChange(change), destroy()}`. The app swaps
// views on navigation and forwards every store change to the current one.
// Views update in place (see dom.reconcile) so typed text, focus and text
// selections survive live updates.

import { approvalCard, confirmButton, leadRoutedCard, questionCard, submitShortcut } from "./cards.js";
import { h, reconcile, replaceChildren } from "./dom.js";
import {
  absoluteTime,
  label,
  MESSAGE_ROLE,
  RECEIPT_STATE,
  relativeTime,
  RUN_PHASE,
  RUN_ROLE,
  shortId,
  THREAD_STATE,
} from "./format.js";
import { HISTORY_PAGE_SIZE, HistoryWindow, messageKey } from "./history.js";
import { renderMarkdown } from "./markdown.js";
import { RereadScheduler } from "./reread.js";
import { fetchAllPages } from "./paging.js";
import { composerRoleFor } from "./store.js";
import { command, describeError, formatBytes, pageRequest, randomUuid } from "./wire.js";

const LIST_PAGE_SIZE = 50;
/** Events without a place within this time cause one re-read of the history. */
const REREAD_DEBOUNCE_MS = 300;
/**
 * Automatic re-reads allowed within REREAD_WINDOW_MS; more wait until the
 * window has room. Every own send and every new run costs one, so this caps
 * a loop that never settles (at most one read per two seconds) rather than
 * normal use.
 */
const MAX_REREADS = 10;
const REREAD_WINDOW_MS = 20_000;

function badges(attention) {
  const out = [];
  if (attention.questions > 0) {
    out.push(
      h("span", {
        class: "badge question",
        text: `質問 ${attention.questions}`,
        attrs: { "aria-label": `未回答の質問 ${attention.questions} 件` },
      }),
    );
  }
  if (attention.approvals > 0) {
    out.push(
      h("span", {
        class: "badge approval",
        text: `承認 ${attention.approvals}`,
        attrs: { "aria-label": `承認待ち ${attention.approvals} 件` },
      }),
    );
  }
  return out;
}

function stateChip(state) {
  return h("span", { class: `chip state-${state}`, text: label(THREAD_STATE, state) });
}

function emptyNote(text) {
  return h("p", { class: "empty", text });
}

/**
 * "さらに読み込む" for a paged listing. A cursor the core no longer knows
 * (`conflict`: expired, or cut from an older state) is dropped and the list is
 * read again from its first page; merging is idempotent.
 */
function pager(load) {
  let cursor = null;
  let done = false;
  const status = h("p", { class: "list-status", attrs: { role: "status" } });
  const more = h("button", { type: "button", class: "button more", text: "さらに読み込む", hidden: true });
  async function next(first) {
    if (done && !first) return;
    more.disabled = true;
    status.textContent = "読み込み中…";
    let result = await load(first ? null : cursor);
    if (!result.ok && result.error.code === "conflict" && !first) {
      status.textContent = "一覧が更新されたため最初から読み直しています…";
      result = await load(null);
    }
    more.disabled = false;
    if (!result.ok) {
      status.textContent = describeError(result.error);
      more.hidden = false;
      more.textContent = "再試行";
      return;
    }
    status.textContent = "";
    cursor = result.value.next_cursor ?? null;
    done = !cursor;
    more.hidden = done;
    more.textContent = "さらに読み込む";
  }
  more.addEventListener("click", () => next(false));
  return { el: h("div", { class: "pager" }, status, more), start: () => next(true) };
}

function attentionSummary(ctx) {
  const total = ctx.store.totalAttention();
  const partial = ctx.store.state.partial;
  if (total.questions + total.approvals === 0 && !partial) return null;
  return h(
    "a",
    { class: "attention-summary", href: "#/inbox" },
    h("strong", { text: "要対応" }),
    h("span", {
      text:
        `未回答の質問 ${total.questions} 件・承認待ち ${total.approvals} 件` +
        (partial ? "（一部のみ表示・読み込み中）" : ""),
    }),
    h("span", { class: "chevron", attrs: { "aria-hidden": "true" }, text: "›" }),
  );
}

// ---- projects -----------------------------------------------------------

export function homeView(ctx) {
  const summary = h("div", { class: "summary-slot" });
  const list = h("ul", { class: "rows", attrs: { "aria-label": "プロジェクト" } });
  const empty = emptyNote("プロジェクトはまだありません。");
  const paging = pager((cursor) =>
    ctx.queryInto("projects", "list_projects", pageRequest(cursor, LIST_PAGE_SIZE)),
  );
  const el = h(
    "section",
    { class: "screen", attrs: { "aria-labelledby": "projects-heading" } },
    summary,
    h("h2", { id: "projects-heading", class: "section-title", text: "プロジェクト" }),
    list,
    empty,
    paging.el,
  );

  function row(project) {
    const li = h("li", { class: "row-item" });
    fill(li, project);
    return li;
  }
  function fill(li, project) {
    replaceChildren(
      li,
      h(
        "a",
        { class: "row", href: `#/p/${encodeURIComponent(project.id)}` },
        h(
          "span",
          { class: "row-main" },
          h("span", { class: "row-title", text: project.display_name || project.id }),
          h("span", { class: "row-sub", text: `スレッド ${project.thread_count} 件` }),
        ),
        h("span", { class: "row-badges" }, badges(ctx.store.projectAttention(project.id))),
        h("span", { class: "chevron", attrs: { "aria-hidden": "true" }, text: "›" }),
      ),
    );
  }
  function render() {
    replaceChildren(summary, attentionSummary(ctx));
    const projects = [...ctx.store.state.projects.values()].sort((a, b) =>
      (a.display_name || a.id).localeCompare(b.display_name || b.id, "ja"),
    );
    reconcile(list, projects, (p) => p.id, row, fill);
    empty.hidden = projects.length > 0;
  }
  render();
  paging.start();
  return { el, title: "evorch", back: null, onChange: render, destroy() {} };
}

// ---- threads of a project -------------------------------------------------

export function projectView(ctx, projectId) {
  const summary = h("div", { class: "summary-slot" });
  const list = h("ul", { class: "rows", attrs: { "aria-label": "スレッド" } });
  const empty = emptyNote("スレッドはまだありません。");
  const paging = pager((cursor) =>
    ctx.queryInto("threads", "list_threads", {
      project: projectId,
      page: pageRequest(cursor, LIST_PAGE_SIZE),
    }),
  );
  const el = h("section", { class: "screen" }, summary, list, empty, paging.el);

  function fill(li, thread) {
    const attention = ctx.store.threadAttention(thread.id);
    replaceChildren(
      li,
      h(
        "a",
        { class: "row", href: `#/t/${encodeURIComponent(thread.id)}` },
        h(
          "span",
          { class: "row-main" },
          h("span", { class: "row-title", text: thread.title || "（無題）" }),
          h(
            "span",
            { class: "row-sub" },
            stateChip(thread.state),
            thread.lead_thread ? h("span", { class: "chip muted", text: "レーン" }) : null,
            h("time", {
              class: "row-time",
              text: relativeTime(thread.updated_at),
              attrs: { title: absoluteTime(thread.updated_at) },
            }),
          ),
        ),
        h("span", { class: "row-badges" }, badges(attention)),
        h("span", { class: "chevron", attrs: { "aria-hidden": "true" }, text: "›" }),
      ),
    );
  }
  function render() {
    replaceChildren(summary, attentionSummary(ctx));
    const threads = ctx.store.threadsOf(projectId);
    reconcile(
      list,
      threads,
      (t) => t.id,
      (t) => {
        const li = h("li", { class: "row-item" });
        fill(li, t);
        return li;
      },
      fill,
    );
    empty.hidden = threads.length > 0;
  }
  const view = {
    el,
    title: ctx.store.state.projects.get(projectId)?.display_name || "スレッド",
    back: "#/",
    onChange() {
      view.title = ctx.store.state.projects.get(projectId)?.display_name || "スレッド";
      ctx.setTitle(view.title);
      render();
    },
    destroy() {},
  };
  render();
  paging.start();
  return view;
}

// ---- inbox ----------------------------------------------------------------

export function inboxView(ctx) {
  const questions = h("div", { class: "cards", attrs: { "aria-label": "未回答の質問" } });
  const approvals = h("div", { class: "cards", attrs: { "aria-label": "承認待ち" } });
  const qCount = h("span", { class: "count" });
  const aCount = h("span", { class: "count" });
  const qEmpty = emptyNote("未回答の質問はありません。");
  const aEmpty = emptyNote("承認待ちはありません。");
  const qPaging = pager((cursor) =>
    ctx.queryInto("questions", "list_questions", {
      pending_only: true,
      page: pageRequest(cursor, LIST_PAGE_SIZE),
    }),
  );
  const aPaging = pager((cursor) =>
    ctx.queryInto("approvals", "list_approvals", {
      pending_only: true,
      page: pageRequest(cursor, LIST_PAGE_SIZE),
    }),
  );
  const partial = h("p", {
    class: "partial-note",
    attrs: { role: "status" },
    text: "コアの一覧が多いため一部のみ表示しています。残りを読み込み中です。",
  });
  const lead = h("div", { class: "cards", attrs: { "aria-label": "Lead が対応中の質問" } });
  const leadCount = h("span", { class: "count" });
  const leadSection = h(
    "details",
    { class: "lead-section" },
    h("summary", {}, "Lead が対応中の質問 ", leadCount),
    lead,
  );
  const el = h(
    "section",
    { class: "screen" },
    partial,
    h("h2", { class: "section-title" }, "未回答の質問 ", qCount),
    questions,
    qEmpty,
    qPaging.el,
    h("h2", { class: "section-title" }, "承認待ち ", aCount),
    approvals,
    aEmpty,
    aPaging.el,
    leadSection,
  );
  function render() {
    renderAttention(ctx, questions, approvals, null, lead);
    const total = ctx.store.totalAttention();
    qCount.textContent = `(${total.questions})`;
    aCount.textContent = `(${total.approvals})`;
    qEmpty.hidden = total.questions > 0;
    aEmpty.hidden = total.approvals > 0;
    const routed = ctx.store.leadRoutedQuestions().length;
    leadCount.textContent = `(${routed})`;
    leadSection.hidden = routed === 0;
    partial.hidden = !ctx.store.state.partial;
  }
  render();
  qPaging.start();
  aPaging.start();
  return { el, title: "要対応", back: "#/", onChange: render, destroy() {} };
}

/**
 * Pending questions first by blocking, then approvals; `threadId` filters.
 * `leadBox` receives the read-only questions routed to a lead: all of them in
 * the inbox, those this thread leads in a thread view.
 */
function renderAttention(ctx, questionBox, approvalBox, threadId, leadBox) {
  const showThread = !threadId;
  const questions = ctx.store
    .pendingQuestions(threadId)
    .sort((a, b) => Number(b.blocking) - Number(a.blocking) || a.id.localeCompare(b.id));
  reconcile(
    questionBox,
    questions,
    (q) => q.id,
    (q) => questionCard(ctx, q, { showThread }),
    (el, q) => el.refresh(q),
  );
  const approvals = ctx.store.pendingApprovals(threadId).sort((a, b) => a.id.localeCompare(b.id));
  reconcile(
    approvalBox,
    approvals,
    (a) => a.id,
    (a) => approvalCard(ctx, a, { showThread }),
    (el, a) => el.refresh(a),
  );
  if (leadBox) {
    reconcile(
      leadBox,
      ctx.store.leadRoutedQuestions(threadId).sort((a, b) => a.id.localeCompare(b.id)),
      (q) => q.id,
      (q) => leadRoutedCard(ctx, q),
      (el, q) => el.refresh(q),
    );
  }
}

// ---- thread ---------------------------------------------------------------

/**
 * Bodies already read, by message (`<thread>/<run>#<index>`, stable across
 * history reads, unlike body ids, which the core issues per read):
 * `{text, next, total, generation}`. A cached body is used for a later
 * BodyRef only when its byte length matches and it was read since the last
 * snapshot (`store.snapshotMark()`): a snapshot follows a gap, a resync or a
 * new core epoch, after which the same run and index may name another
 * message. Shared by views.
 */
const bodies = new Map();

/** Drop bodies read before the snapshot `generation`. */
function forgetOldBodies(generation) {
  for (const [key, body] of bodies) {
    if (body.generation !== generation) bodies.delete(key);
  }
}

function bodyKey(message) {
  return `${message.thread}/${messageKey(message)}`;
}
/** Messages being sent, by thread: they outlive the view while it is away. */
const outbox = new Map();

function draftKey(threadId) {
  return `evorch:draft:${threadId}`;
}

function loadDraft(threadId) {
  try {
    return localStorage.getItem(draftKey(threadId)) ?? "";
  } catch {
    return "";
  }
}

function saveDraft(threadId, text) {
  try {
    if (text) localStorage.setItem(draftKey(threadId), text);
    else localStorage.removeItem(draftKey(threadId));
  } catch {
    // Storage may be disabled (private mode); the draft just is not kept.
  }
}

function nearBottom() {
  const s = document.scrollingElement;
  return s.scrollHeight - s.scrollTop - s.clientHeight < 160;
}

function scrollToBottom() {
  const s = document.scrollingElement;
  s.scrollTop = s.scrollHeight;
}

export function threadView(ctx, threadId) {
  const meta = h("div", { class: "thread-meta" });
  const runsBox = h("div", { class: "runs" });
  const stop = confirmButton({
    text: "停止",
    confirmText: "もう一度押して停止",
    className: "button danger small",
    ariaLabel: "このスレッドの実行を停止",
    onConfirm: () => stopChat(),
  });
  const questionBox = h("div", { class: "cards", attrs: { "aria-label": "このスレッドの未回答の質問" } });
  const approvalBox = h("div", { class: "cards", attrs: { "aria-label": "このスレッドの承認待ち" } });
  const older = h("button", { type: "button", class: "button older", text: "以前のメッセージを読み込む", hidden: true });
  // Shown instead of re-reading while the person reads further up.
  const newMessages = h("button", {
    type: "button",
    class: "button new-messages",
    text: "新しいメッセージがあります",
    hidden: true,
    attrs: { "aria-label": "新しいメッセージがあります。末尾へ移動して表示します" },
  });
  const historyStatus = h("p", { class: "history-status", attrs: { role: "status" } });
  const reload = h("button", { type: "button", class: "button small", text: "履歴を読み込み直す", hidden: true });
  const leadBox = h("div", { class: "cards", attrs: { "aria-label": "Lead が対応中の質問" } });
  const list = h("div", { class: "messages", attrs: { role: "log", "aria-label": "メッセージ" } });
  const sending = h("div", { class: "outbox", attrs: { "aria-label": "送信中のメッセージ" } });

  const input = h("textarea", {
    class: "composer-input",
    rows: 1,
    value: loadDraft(threadId),
    attrs: {
      "aria-label": "メッセージ",
      placeholder: "メッセージを入力",
      title: "Ctrl+Enter（Mac は ⌘+Enter）でも送信できます",
      enterkeyhint: "enter",
    },
  });
  const roleSelect = h(
    "select",
    { class: "composer-role", id: `composer-role-${threadId}` },
    h("option", { value: "", text: "判定中…", disabled: true }),
    h("option", { value: "worker", text: "Worker" }),
    h("option", { value: "orchestrator", text: "Orchestrator" }),
  );
  const send = h("button", { type: "button", class: "button primary send", text: "送信" });
  // Nothing is selected until the role is known (see renderHead).
  roleSelect.value = "";
  const composerStatus = h("p", { class: "composer-status", attrs: { role: "status", "aria-live": "polite" } });
  const composer = h(
    "div",
    { class: "composer", attrs: { role: "group", "aria-label": "メッセージの送信" } },
    newMessages,
    composerStatus,
    h(
      "div",
      { class: "composer-meta" },
      h("label", { class: "composer-role-label", htmlFor: roleSelect.id, text: "送信先" }),
      roleSelect,
    ),
    h("div", { class: "composer-row" }, input, send),
  );
  // Grow with the text up to the CSS max-height (CSSOM writes are not inline
  // style attributes, so the Content-Security-Policy allows them).
  const autosize = () => {
    input.style.height = "auto";
    input.style.height = `${input.scrollHeight + 2}px`;
  };
  input.addEventListener("input", autosize);
  requestAnimationFrame(autosize);

  const el = h(
    "section",
    { class: "screen thread" },
    h("div", { class: "thread-head" }, meta, h("div", { class: "thread-actions" }, stop)),
    runsBox,
    questionBox,
    approvalBox,
    leadBox,
    older,
    historyStatus,
    reload,
    list,
    sending,
    composer,
  );

  /** The history window on screen, and the one being built to replace it. */
  let history = null;
  /** The person is at the end of the history (see onScroll). */
  let following = true;

  function toBottom() {
    following = true;
    scrollToBottom();
  }
  let building = null;
  /** Re-reads of the newest page for events without a place on screen. */
  const rereads = new RereadScheduler({
    run: () => {
      if (following) rebuildHistory({ reread: true });
      else deferReread();
    },
    onPaused: (waitMs) => {
      historyStatus.textContent = `履歴の更新が続いているため、自動の読み直しを一時停止しています（約 ${Math.ceil(waitMs / 1000)} 秒後に再開）。`;
      reload.hidden = false;
    },
    debounceMs: REREAD_DEBOUNCE_MS,
    max: MAX_REREADS,
    windowMs: REREAD_WINDOW_MS,
  });
  let runsSignature = null;
  /** Reading this thread's runs: "loading", "complete" or a failure. */
  let runsLoad = { state: "loading" };
  let runsGeneration = 0;
  /** The role was chosen by the person or determined from the root run. */
  let roleTouched = false;
  let roleDetermined = false;
  let destroyed = false;
  const observer =
    "IntersectionObserver" in globalThis
      ? new IntersectionObserver((entries) => {
          for (const entry of entries) {
            if (!entry.isIntersecting) continue;
            observer.unobserve(entry.target);
            entry.target.autoLoad?.();
          }
        })
      : null;

  // ---- header -----------------------------------------------------------

  function thread() {
    return ctx.store.state.threads.get(threadId);
  }

  function renderHead() {
    const t = thread();
    const view = ctx.store.state.threadViews.get(threadId);
    replaceChildren(
      meta,
      t ? stateChip(t.state) : null,
      t?.lead_thread
        ? h("a", { class: "chip muted", href: `#/t/${encodeURIComponent(t.lead_thread)}`, text: "レーン（lead へ）" })
        : null,
      t
        ? h("time", { class: "row-time", text: relativeTime(t.updated_at), attrs: { title: absoluteTime(t.updated_at) } })
        : null,
      view?.queued_follow_ups ? h("span", { class: "chip muted", text: `追送待ち ${view.queued_follow_ups}` }) : null,
    );
    const active = ctx.store.activeRuns(threadId);
    const running = active.length > 0 || (t && t.state !== "idle" && t.state !== "failed");
    stop.hidden = !running;
    // Keep the run list open across live updates if the user opened it, and
    // rebuild it only when the runs change so an armed stop button survives.
    // Until every page of the thread's runs is read, the list may miss runs
    // that can be stopped individually; say so instead of looking complete.
    const signature =
      `${runsLoad.state}|` + active.map((run) => `${run.id}:${run.phase}`).join(",");
    const note =
      runsLoad.state === "loading"
        ? h("p", { class: "runs-note", attrs: { role: "status" }, text: "run の一覧を読み込み中…" })
        : runsLoad.state === "failed"
          ? h(
              "p",
              { class: "runs-note", attrs: { role: "status" } },
              `run の一覧を最後まで読み込めませんでした（${describeError(runsLoad.error)}）。個別に停止できる run が欠けている可能性があります。`,
              h("button", {
                type: "button",
                class: "button small",
                text: "再読み込み",
                on: { click: () => loadRuns() },
              }),
            )
          : null;
    if (signature !== runsSignature && active.length === 0) {
      runsSignature = signature;
      replaceChildren(runsBox, note);
    } else if (signature !== runsSignature) {
      runsSignature = signature;
      const open = runsBox.querySelector("details")?.open ?? false;
      replaceChildren(
        runsBox,
        note,
        h(
          "details",
          { class: "runs-details", open },
          h("summary", {
            text: `実行中の run (${active.length}${runsLoad.state === "complete" ? "" : "・一部"})`,
          }),
          h(
            "ul",
            { class: "run-list" },
            active.map((run) =>
              h(
                "li",
                { class: "run-item" },
                h("span", { class: "run-label", text: `${label(RUN_ROLE, run.role)} ${shortId(run.id)}` }),
                h("span", { class: "chip", text: label(RUN_PHASE, run.phase) }),
                confirmButton({
                  text: "この run を停止",
                  confirmText: "もう一度押して停止",
                  className: "button danger small",
                  ariaLabel: `run ${run.id} を停止`,
                  onConfirm: () => stopRun(run.id),
                }),
              ),
            ),
          ),
        ),
      );
    }
    // The role is set once from the root run. While the thread view or the
    // root run is unknown (first load, or rebuilt after a snapshot) the
    // current choice stays; nothing is sent before one is made.
    if (!roleTouched && !roleDetermined) {
      const role = composerRoleFor(view, ctx.store.state.runs);
      if (role === "orchestrator" || role === "worker") {
        roleSelect.value = role;
        roleDetermined = true;
      } else if (role === "choose") {
        roleSelect.options[0].textContent = "選択してください";
      }
    }
    send.disabled = roleSelect.value === "";
    renderAttention(ctx, questionBox, approvalBox, threadId, leadBox);
  }

  async function stopChat() {
    composerStatus.textContent = "停止を要求しています…";
    const result = await ctx.api.sendCommand(command("stop_chat", { thread: threadId }));
    composerStatus.textContent = result.ok ? "停止を要求しました。" : describeError(result.error);
  }

  async function stopRun(runId) {
    composerStatus.textContent = "停止を要求しています…";
    const result = await ctx.api.sendCommand(command("stop_run", { thread: threadId, run: runId }));
    composerStatus.textContent = result.ok ? "停止を要求しました。" : describeError(result.error);
  }

  // ---- messages ---------------------------------------------------------

  function messageElement(message) {
    const role = message.role;
    const run = ctx.store.state.runs.get(message.run);
    const content = h("div", { class: "msg-body selectable" });
    const article = h(
      "article",
      { class: `msg msg-${role}` },
      h(
        "header",
        { class: "msg-head" },
        h("span", { class: "msg-role", text: label(MESSAGE_ROLE, role) }),
        run && run.role !== "orchestrator" && role !== "user"
          ? h("span", { class: "msg-run", text: `${label(RUN_ROLE, run.role)} ${shortId(run.id)}` })
          : null,
      ),
      content,
    );
    if (message.content.type === "inline") {
      fillText(content, role, message.content.payload.text);
    } else {
      article.setBodyRef = bodyPlaceholder(content, role, message.content.payload, bodyKey(message));
    }
    article.generation = ctx.store.snapshotMark();
    return article;
  }

  /** A re-read gives a kept message element its new BodyRef. */
  function updateMessage(el, message) {
    if (message.content.type === "body") el.setBodyRef?.(message.content.payload);
  }

  function fillText(container, role, text, complete = true) {
    if (role === "tool") {
      const lines = text.split("\n").length;
      replaceChildren(
        container,
        h(
          "details",
          { class: "tool-output" },
          h("summary", { text: `ツール出力（${lines} 行${complete ? "" : "・途中まで"}）` }),
          h("pre", { text }),
        ),
      );
    } else if (role === "user") {
      replaceChildren(container, h("p", { class: "plain", text }));
    } else {
      replaceChildren(container, renderMarkdown(text));
    }
  }

  // The body of one message. `ref` is replaced by later history reads (new
  // body id); what was read is kept under `key` and continued with the
  // current id.
  function bodyPlaceholder(container, role, firstRef, key) {
    let ref = firstRef;
    /** The snapshot the current BodyRef came from; an older one may name another body. */
    let refGeneration = ctx.store.snapshotMark();
    const status = h("p", { class: "body-status", attrs: { role: "status" } });
    const load = h("button", {
      type: "button",
      class: "button small",
      text: `本文を読み込む（${formatBytes(ref.total_bytes)}）`,
    });
    const rendered = h("div", { class: "body-text" });
    const more = h("button", { type: "button", class: "button small", text: "続きを読み込む", hidden: true });
    replaceChildren(container, rendered, status, load, more);

    /** The cached body, if it is this body (same length, same snapshot). */
    function cachedBody() {
      const cached = bodies.get(key);
      if (cached && (cached.total !== ref.total_bytes || cached.generation !== ctx.store.snapshotMark())) {
        bodies.delete(key);
        return undefined;
      }
      return cached;
    }

    function show() {
      const cached = cachedBody();
      if (!cached) return;
      fillText(rendered, role, cached.text, !cached.next);
      load.hidden = true;
      more.hidden = !cached.next;
      if (cached.next) {
        const left = Number(ref.total_bytes) - Number(cached.next);
        more.textContent = `続きを読み込む（残り ${formatBytes(String(left))}）`;
      }
    }

    async function read() {
      const generation = ctx.store.snapshotMark();
      if (refGeneration !== generation) {
        status.textContent = "履歴を読み込み直しています…";
        return;
      }
      const cached = cachedBody();
      const offset = cached ? cached.next : "0";
      if (offset === null) return;
      load.disabled = true;
      more.disabled = true;
      status.textContent = "読み込み中…";
      const result = await ctx.api.query("read_body", {
        body_id: ref.body_id,
        offset,
        max_bytes: ctx.maxBodyChunk(),
      });
      load.disabled = false;
      more.disabled = false;
      if (!result.ok) {
        status.textContent = describeError(result.error);
        return;
      }
      status.textContent = "";
      if (generation !== ctx.store.snapshotMark()) {
        // Asked for before a snapshot: it may be another message's body now.
        // Nothing is kept from this answer; a body read on sight is watched
        // again so that it is read anew if this element stays.
        if (container.autoLoad) observer?.observe(container);
        return;
      }
      const chunk = result.value;
      if (chunk.total_bytes !== ref.total_bytes || (cached && bodies.get(key) !== cached)) {
        // Not the body that was being read: start again from its beginning.
        bodies.delete(key);
        status.textContent = "本文が更新されました。もう一度読み込んでください。";
        load.hidden = false;
        more.hidden = true;
        replaceChildren(rendered);
        return;
      }
      bodies.set(key, {
        text: (cached?.text ?? "") + chunk.text,
        next: chunk.next_offset ?? null,
        total: chunk.total_bytes,
        generation,
      });
      show();
    }

    load.addEventListener("click", read);
    more.addEventListener("click", read);
    if (cachedBody()) {
      show();
    } else if (observer && Number(ref.total_bytes) <= ctx.maxBodyChunk()) {
      // A body that fits in one chunk is read when it scrolls into view.
      container.autoLoad = read;
      observer.observe(container);
    }
    return (next) => {
      const changed = next.total_bytes !== ref.total_bytes;
      ref = next;
      refGeneration = ctx.store.snapshotMark();
      if (changed) {
        bodies.delete(key);
        replaceChildren(rendered);
        status.textContent = "";
        load.textContent = `本文を読み込む（${formatBytes(ref.total_bytes)}）`;
        load.hidden = false;
        more.hidden = true;
      }
    };
  }

  function renderMessages() {
    if (!history) return;
    const messages = history.messages();
    // An element is reused only within the snapshot it was made in: after a
    // snapshot the same run and index may name another message.
    const generation = ctx.store.snapshotMark();
    for (const child of [...list.children]) {
      if (child.generation !== generation) child.remove();
    }
    reconcile(list, messages, messageKey, messageElement, updateMessage);
    older.hidden = !history.hasOlder;
  }

  /**
   * The first message at the top of the viewport (below the sticky header)
   * and its offset, to put it back at the same place after the list changes.
   */
  function scrollAnchor() {
    const top = document.querySelector(".chrome")?.getBoundingClientRect().bottom ?? 0;
    for (const el of list.children) {
      const rect = el.getBoundingClientRect();
      if (rect.bottom > top) return { key: el.dataset.key, offset: rect.top };
    }
    return null;
  }

  function restoreAnchor(anchor) {
    const el = anchor && [...list.children].find((child) => child.dataset.key === anchor.key);
    if (el) window.scrollBy(0, el.getBoundingClientRect().top - anchor.offset);
  }

  // Rebuild the history from scratch (first open, after a snapshot: the
  // stream had a gap, so the messages on screen may be incomplete; or a
  // re-read because an event had no known place). Events that arrive while
  // the walk runs are kept by the new window itself and applied after the
  // walk; the old window's live messages are dropped. After a snapshot the
  // list is rebuilt; a re-read keeps the elements of messages still there
  // (with what their bodies showed) and as many pages as were loaded. Unless
  // the view is at the bottom when the result is shown, the message at the
  // top of the screen stays where it was.
  async function rebuildHistory({ reread = false, asked = false } = {}) {
    if (reread && history && history.generation !== ctx.store.snapshotMark()) {
      // The history on screen is from before a snapshot whose rebuild has
      // not succeeded yet: a retry is that rebuild, not a re-read.
      reread = false;
    }
    if (reread && !asked && building && !building.reread) {
      // A rebuild after a snapshot is running; it reads the newest page
      // anyway, and must not be replaced by a re-read that may wait.
      building.unplaced = true;
      return;
    }
    const keepPages = reread && history ? history.pages.length : 1;
    const next = new HistoryWindow((cursor) =>
      ctx.api.query("read_committed_history", {
        direction: "backward",
        thread: threadId,
        page: pageRequest(cursor, HISTORY_PAGE_SIZE),
      }),
    );
    next.reread = reread;
    next.generation = ctx.store.snapshotMark();
    building = next;
    // A re-read shows no status line: it sits above the list, and adding or
    // removing it would move what is on screen.
    if (!reread) historyStatus.textContent = "履歴を読み込み中…";
    reload.hidden = true;
    let result = await next.open();
    if (result.resync) next.unplaced = true;
    while (result.ok && next.pages.length < keepPages && next.hasOlder && building === next && !destroyed) {
      result = await next.loadOlder();
    }
    if (destroyed || building !== next) return;
    building = null;
    if (!result.ok) {
      historyStatus.textContent = describeError(result.error);
      reload.hidden = false;
      return;
    }
    if (reread && !asked && !following) {
      // The person scrolled up while it was read: replacing the list now
      // could drop what they read. Wait until they are back at the bottom.
      deferReread();
      return;
    }
    historyStatus.textContent = "";
    if (reread) {
      rereadDeferred = false;
      newMessages.hidden = true;
    }
    const first = history === null;
    const follow = first || following;
    const anchor = follow ? null : scrollAnchor();
    history = next;
    if (!reread) list.replaceChildren();
    renderMessages();
    if (follow) toBottom();
    else restoreAnchor(anchor);
    // The page is right for its cut; an event held during the read that had
    // no place only means another read is due.
    if (next.unplaced) requestReread();
  }

  // Re-read the newest page for events without a place, but only at the
  // bottom of the history. Further up the person is reading: the list stays
  // as it is and "新しいメッセージがあります" offers to go to the end.
  let rereadDeferred = false;

  function requestReread() {
    if (destroyed) return;
    if (following) rereads.request();
    else deferReread();
  }

  function deferReread() {
    rereadDeferred = true;
    newMessages.hidden = false;
  }

  function resumeReread() {
    if (!rereadDeferred || destroyed) return;
    rereadDeferred = false;
    newMessages.hidden = true;
    rereads.request();
  }

  newMessages.addEventListener("click", () => {
    toBottom();
    resumeReread();
  });
  // Whether the person follows the end of the history. It changes only when
  // the page scrolls (by the person or by toBottom), not when content above
  // grows, so a body loading in does not count as scrolling away.
  const onScroll = () => {
    following = nearBottom();
    if (rereadDeferred && following) resumeReread();
  };
  window.addEventListener("scroll", onScroll, { passive: true });

  reload.addEventListener("click", () => {
    rereads.reset();
    rebuildHistory({ reread: history !== null, asked: true });
  });

  older.addEventListener("click", async () => {
    if (!history) return;
    const s = document.scrollingElement;
    const before = s.scrollHeight - s.scrollTop;
    older.disabled = true;
    const result = await history.loadOlder();
    older.disabled = false;
    if (!result.ok) {
      if (result.error.code === "conflict") {
        historyStatus.textContent = "履歴が更新されたため読み込み直します。";
        await rebuildHistory();
      } else {
        historyStatus.textContent = describeError(result.error);
      }
      return;
    }
    renderMessages();
    s.scrollTop = s.scrollHeight - before;
  });

  // ---- sending ----------------------------------------------------------
  //
  // Each outgoing message keeps its submission id and request id:
  // - outcome unknown: ask the core (GetCommandReceipt) and resend with the
  //   same request id and submission id, so the core can deduplicate;
  // - refused (a decided error): resend with a new request id and the same
  //   submission id, which the core never accepted;
  // - dropped (its run ended before reading it): the core already took that
  //   submission id, so a resend is a new submission with a new id.
  // The bubble goes away when the input receipt of its submission id says the
  // message was delivered or withdrawn.

  function pendingSends() {
    if (!outbox.has(threadId)) outbox.set(threadId, []);
    return outbox.get(threadId);
  }

  function renderOutbox() {
    const items = pendingSends();
    for (const item of [...items]) {
      const receipt = ctx.store.state.receipts.get(item.submissionId);
      if (receipt && (receipt.state === "delivered" || receipt.state === "withdrawn")) {
        items.splice(items.indexOf(item), 1);
      }
    }
    reconcile(
      sending,
      items,
      (item) => item.submissionId,
      (item) => outgoingElement(item),
      (el, item) => el.refresh(item),
    );
  }

  function outgoingElement(item) {
    const state = h("p", { class: "outgoing-state", attrs: { role: "status" } });
    const actions = h("div", { class: "outgoing-actions" });
    const el = h(
      "article",
      { class: "msg msg-user outgoing" },
      h("header", { class: "msg-head" }, h("span", { class: "msg-role", text: "あなた（送信中）" })),
      h("div", { class: "msg-body selectable" }, h("p", { class: "plain", text: item.text })),
      state,
      actions,
    );
    el.refresh = (current) => {
      const receipt = ctx.store.state.receipts.get(current.submissionId);
      const dropped = current.state === "accepted" && receipt?.state === "dropped";
      let text;
      switch (current.state) {
        case "sending":
          text = "送信中…";
          break;
        case "accepted":
          text = receipt ? label(RECEIPT_STATE, receipt.state) : "受付済み";
          break;
        case "unknown":
          text = "届いたか確認できませんでした。確認してから再送します。";
          break;
        default:
          text = current.error ? describeError(current.error) : "送信できませんでした。";
      }
      state.textContent = text;
      const button = (label, action, onClick) =>
        h("button", {
          type: "button",
          class: "button small",
          text: label,
          dataset: { action },
          on: { click: onClick },
        });
      const buttons = [];
      if (current.state === "unknown") {
        buttons.push(button("確認して再送", "confirm", () => deliver(current, "confirm")));
      }
      if (current.state === "failed") {
        buttons.push(button("再送", "retry", () => deliver(current, "retry")));
      }
      if (dropped) {
        buttons.push(button("新しいメッセージとして再送", "resubmit", () => deliver(current, "resubmit")));
      }
      // Moving the text back to the composer would send it under a new
      // submission id, so it is offered only once the old one cannot arrive.
      if (current.state === "failed" || dropped) {
        buttons.push(
          button("入力欄に戻す", "restore", () => {
            input.value = input.value ? `${input.value}\n${current.text}` : current.text;
            saveDraft(threadId, input.value);
            autosize();
            removeOutgoing(current.submissionId);
            input.focus();
          }),
        );
      }
      replaceChildren(actions, buttons);
    };
    el.refresh(item);
    return el;
  }

  function removeOutgoing(submissionId) {
    const items = pendingSends();
    const index = items.findIndex((item) => item.submissionId === submissionId);
    if (index >= 0) items.splice(index, 1);
    renderOutbox();
  }

  /** `mode`: "send" (first time), "confirm", "retry" or "resubmit" (see above). */
  async function deliver(item, mode = "send") {
    if (mode === "retry" || mode === "resubmit") item.requestId = ctx.api.newRequestId();
    if (mode === "resubmit") item.submissionId = `sub-${randomUuid()}`;
    item.state = "sending";
    item.error = null;
    renderOutbox();
    const result = await ctx.api.sendCommand(
      command("send_chat", {
        project: item.project,
        thread: threadId,
        submission_id: item.submissionId,
        composer_role: item.role,
        text: item.text,
      }),
      { requestId: item.requestId, confirmFirst: mode === "confirm" },
    );
    if (result.ok) {
      item.state = "accepted";
    } else if (result.unknown) {
      item.state = "unknown";
      item.error = result.error;
    } else {
      item.state = "failed";
      item.error = result.error;
    }
    renderOutbox();
  }

  function submit() {
    const text = input.value;
    if (text.trim() === "") return;
    const t = thread();
    if (!t) {
      composerStatus.textContent = "スレッドの情報を読み込めていないため送信できません。";
      return;
    }
    if (roleSelect.value === "") {
      composerStatus.textContent = "送信先（Worker / Orchestrator）を選んでから送信してください。";
      roleSelect.focus();
      return;
    }
    const item = {
      submissionId: `sub-${randomUuid()}`,
      requestId: ctx.api.newRequestId(),
      project: t.project,
      text,
      role: roleSelect.value,
      state: "sending",
      error: null,
    };
    pendingSends().push(item);
    input.value = "";
    autosize();
    saveDraft(threadId, "");
    composerStatus.textContent = "";
    deliver(item);
    toBottom();
  }

  send.addEventListener("click", submit);
  submitShortcut(input, submit);
  input.addEventListener("input", () => saveDraft(threadId, input.value));
  roleSelect.addEventListener("change", () => {
    roleTouched = true;
    send.disabled = roleSelect.value === "";
    composerStatus.textContent = "";
  });

  // ---- store changes ----------------------------------------------------

  function onMessage(message) {
    if (message.thread !== threadId) return;
    if (building) {
      // The window being built holds it until its walk ends; one without a
      // place there asks for another read once the walk is adopted.
      if (building.append(message) === "resync") building.unplaced = true;
      return;
    }
    if (!history) return;
    const placed = history.append(message);
    if (placed === "added") {
      renderMessages();
      if (following) toBottom();
    } else if (placed === "resync") {
      requestReread();
    }
  }

  async function loadThread() {
    const issued = ctx.store.mark();
    const result = await ctx.api.query("get_thread", { thread: threadId });
    if (destroyed) return false;
    if (result.ok) {
      ctx.store.setThreadView(result.value, issued);
    } else if (result.error.code === "not_found") {
      replaceChildren(el, emptyNote("このスレッドは見つかりません。"));
      return false;
    } else {
      historyStatus.textContent = describeError(result.error);
    }
    await loadRuns();
    return true;
  }

  /**
   * Read every page of this thread's runs (expired cursors restart). A newer
   * read (after a snapshot, or a manual reload) supersedes an older one, so a
   * slow earlier read cannot report "complete" for the newer state.
   */
  async function loadRuns() {
    const mine = (runsGeneration += 1);
    runsLoad = { state: "loading" };
    renderHead();
    const result = await fetchAllPages((cursor) =>
      ctx.queryInto("runs", "list_runs", { thread: threadId, page: pageRequest(cursor, 200) }),
    );
    if (destroyed || mine !== runsGeneration) return;
    runsLoad = result.ok ? { state: "complete" } : { state: "failed", error: result.error };
    renderHead();
  }

  const view = {
    el,
    title: thread()?.title || "スレッド",
    back: thread() ? `#/p/${encodeURIComponent(thread().project)}` : "#/",
    onChange(change) {
      switch (change.type) {
        case "message":
          onMessage(change.message);
          return;
        case "snapshot":
          // A snapshot follows a gap in the stream: messages committed in the
          // gap are not in it, and the thread view was dropped with the state.
          // The run list is stale from now on, not only once it is re-read.
          runsGeneration += 1;
          runsLoad = { state: "loading" };
          forgetOldBodies(ctx.store.snapshotMark());
          rebuildHistory();
          loadThread();
          break;
        case "receipt":
          if (change.receipt.thread === threadId) renderOutbox();
          break;
        default:
          break;
      }
      const t = thread();
      if (t) {
        view.title = t.title || "スレッド";
        view.back = `#/p/${encodeURIComponent(t.project)}`;
        ctx.setTitle(view.title);
        ctx.setBack(view.back);
      }
      renderHead();
    },
    destroy() {
      destroyed = true;
      rereads.dispose();
      window.removeEventListener("scroll", onScroll);
      observer?.disconnect();
    },
  };

  renderHead();
  renderOutbox();
  rebuildHistory();
  loadThread();
  return view;
}
