// Screens: project list, thread list, thread, and the "needs attention" inbox.
//
// A view is `{el, title, back, onChange(change), destroy()}`. The app swaps
// views on navigation and forwards every store change to the current one.
// Views update in place (see dom.reconcile) so typed text, focus and text
// selections survive live updates.

import { approvalCard, confirmButton, questionCard, submitShortcut } from "./cards.js";
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
import { HistoryWindow, messageKey, WALK_PAGE_SIZE } from "./history.js";
import { renderMarkdown } from "./markdown.js";
import { command, describeError, formatBytes, pageRequest, randomUuid } from "./wire.js";

const LIST_PAGE_SIZE = 50;

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

/** "さらに読み込む" for a paged listing. */
function pager(load) {
  let cursor = null;
  let done = false;
  const status = h("p", { class: "list-status", attrs: { role: "status" } });
  const more = h("button", { type: "button", class: "button more", text: "さらに読み込む", hidden: true });
  async function next(first) {
    if (done && !first) return;
    more.disabled = true;
    status.textContent = "読み込み中…";
    const result = await load(first ? null : cursor);
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
  if (total.questions + total.approvals === 0) return null;
  return h(
    "a",
    { class: "attention-summary", href: "#/inbox" },
    h("strong", { text: "要対応" }),
    h("span", { text: `未回答の質問 ${total.questions} 件・承認待ち ${total.approvals} 件` }),
    h("span", { class: "chevron", attrs: { "aria-hidden": "true" }, text: "›" }),
  );
}

// ---- projects -----------------------------------------------------------

export function homeView(ctx) {
  const summary = h("div", { class: "summary-slot" });
  const list = h("ul", { class: "rows", attrs: { "aria-label": "プロジェクト" } });
  const empty = emptyNote("プロジェクトはまだありません。");
  const paging = pager(async (cursor) => {
    const result = await ctx.api.query("list_projects", pageRequest(cursor, LIST_PAGE_SIZE));
    if (result.ok) ctx.store.mergePage("projects", result.value.items);
    return result;
  });
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
  const paging = pager(async (cursor) => {
    const result = await ctx.api.query("list_threads", {
      project: projectId,
      page: pageRequest(cursor, LIST_PAGE_SIZE),
    });
    if (result.ok) ctx.store.mergePage("threads", result.value.items);
    return result;
  });
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
  const qPaging = pager(async (cursor) => {
    const result = await ctx.api.query("list_questions", {
      pending_only: true,
      page: pageRequest(cursor, LIST_PAGE_SIZE),
    });
    if (result.ok) ctx.store.mergePage("questions", result.value.items);
    return result;
  });
  const aPaging = pager(async (cursor) => {
    const result = await ctx.api.query("list_approvals", {
      pending_only: true,
      page: pageRequest(cursor, LIST_PAGE_SIZE),
    });
    if (result.ok) ctx.store.mergePage("approvals", result.value.items);
    return result;
  });
  const el = h(
    "section",
    { class: "screen" },
    h("h2", { class: "section-title" }, "未回答の質問 ", qCount),
    questions,
    qEmpty,
    qPaging.el,
    h("h2", { class: "section-title" }, "承認待ち ", aCount),
    approvals,
    aEmpty,
    aPaging.el,
  );
  function render() {
    renderAttention(ctx, questions, approvals, null);
    const total = ctx.store.totalAttention();
    qCount.textContent = `(${total.questions})`;
    aCount.textContent = `(${total.approvals})`;
    qEmpty.hidden = total.questions > 0;
    aEmpty.hidden = total.approvals > 0;
  }
  render();
  qPaging.start();
  aPaging.start();
  return { el, title: "要対応", back: "#/", onChange: render, destroy() {} };
}

/** Pending questions first by blocking, then approvals; `threadId` filters. */
function renderAttention(ctx, questionBox, approvalBox, threadId) {
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
}

// ---- thread ---------------------------------------------------------------

/** Bodies already read, by body id: `{text, next, total}`. Shared by views. */
const bodies = new Map();
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
  const historyStatus = h("p", { class: "history-status", attrs: { role: "status" } });
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
    h("option", { value: "worker", text: "Worker" }),
    h("option", { value: "orchestrator", text: "Orchestrator" }),
  );
  const send = h("button", { type: "button", class: "button primary send", text: "送信" });
  const composerStatus = h("p", { class: "composer-status", attrs: { role: "status", "aria-live": "polite" } });
  const composer = h(
    "div",
    { class: "composer", attrs: { role: "group", "aria-label": "メッセージの送信" } },
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
    older,
    historyStatus,
    list,
    sending,
    composer,
  );

  let history = null;
  let runsSignature = null;
  /** Messages that arrive before the first walk ends. */
  const pendingMessages = [];
  let generation = 0;
  let roleTouched = false;
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
    const signature = active.map((run) => `${run.id}:${run.phase}`).join(",");
    if (signature !== runsSignature && active.length === 0) {
      runsSignature = signature;
      replaceChildren(runsBox);
    } else if (signature !== runsSignature) {
      runsSignature = signature;
      const open = runsBox.querySelector("details")?.open ?? false;
      replaceChildren(
        runsBox,
        h(
          "details",
          { class: "runs-details", open },
          h("summary", { text: `実行中の run (${active.length})` }),
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
    if (!roleTouched) {
      const root = view?.root_run ? ctx.store.state.runs.get(view.root_run) : null;
      roleSelect.value = root?.role === "orchestrator" ? "orchestrator" : "worker";
    }
    renderAttention(ctx, questionBox, approvalBox, threadId);
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
      bodyPlaceholder(content, role, message.content.payload);
    }
    return article;
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

  function bodyPlaceholder(container, role, ref) {
    const status = h("p", { class: "body-status", attrs: { role: "status" } });
    const load = h("button", {
      type: "button",
      class: "button small",
      text: `本文を読み込む（${formatBytes(ref.total_bytes)}）`,
    });
    const rendered = h("div", { class: "body-text" });
    const more = h("button", { type: "button", class: "button small", text: "続きを読み込む", hidden: true });
    replaceChildren(container, rendered, status, load, more);

    function show() {
      const cached = bodies.get(ref.body_id);
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
      const cached = bodies.get(ref.body_id);
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
      const chunk = result.value;
      bodies.set(ref.body_id, {
        text: (cached?.text ?? "") + chunk.text,
        next: chunk.next_offset ?? null,
        total: chunk.total_bytes,
      });
      show();
    }

    load.addEventListener("click", read);
    more.addEventListener("click", read);
    if (bodies.has(ref.body_id)) {
      show();
    } else if (observer && Number(ref.total_bytes) <= ctx.maxBodyChunk()) {
      // A body that fits in one chunk is read when it scrolls into view.
      container.autoLoad = read;
      observer.observe(container);
    }
  }

  function renderMessages() {
    if (!history) return;
    const messages = history.messages();
    reconcile(list, messages, messageKey, messageElement, null);
    older.hidden = !history.hasOlder;
  }

  async function openHistory() {
    const mine = (generation += 1);
    const next = new HistoryWindow((cursor) =>
      ctx.api.query("read_committed_history", {
        thread: threadId,
        page: pageRequest(cursor, WALK_PAGE_SIZE),
      }),
    );
    historyStatus.textContent = "履歴を読み込み中…";
    const result = await next.open();
    if (destroyed || mine !== generation) return;
    if (!result.ok) {
      historyStatus.textContent = describeError(result.error);
      return;
    }
    historyStatus.textContent = "";
    // Events that arrived while the old window was open belong to it.
    if (history) for (const m of history.live) next.append(m);
    history = next;
    list.replaceChildren();
    renderMessages();
    scrollToBottom();
  }

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
        await openHistory();
      } else {
        historyStatus.textContent = describeError(result.error);
      }
      return;
    }
    renderMessages();
    s.scrollTop = s.scrollHeight - before;
  });

  // ---- sending ----------------------------------------------------------

  function pendingSends() {
    if (!outbox.has(threadId)) outbox.set(threadId, []);
    return outbox.get(threadId);
  }

  function renderOutbox() {
    const items = pendingSends();
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
      let text;
      switch (current.state) {
        case "sending":
          text = "送信中…";
          break;
        case "accepted":
          text = receipt ? label(RECEIPT_STATE, receipt.state) : "受付済み";
          break;
        case "unknown":
          text = "届いたか確認できませんでした。";
          break;
        default:
          text = current.error ? describeError(current.error) : "送信できませんでした。";
      }
      state.textContent = text;
      const buttons = [];
      if (current.state === "unknown") {
        buttons.push(
          h("button", {
            type: "button",
            class: "button small",
            text: "確認して再送",
            on: { click: () => deliver(current, { confirmFirst: true }) },
          }),
        );
      }
      if (current.state === "failed" || receipt?.state === "dropped") {
        buttons.push(
          h("button", {
            type: "button",
            class: "button small",
            text: "再送",
            on: { click: () => deliver(current, { fresh: true }) },
          }),
        );
      }
      if (current.state === "failed" || current.state === "unknown" || receipt?.state === "dropped") {
        buttons.push(
          h("button", {
            type: "button",
            class: "button small",
            text: "入力欄に戻す",
            on: {
              click: () => {
                input.value = input.value ? `${input.value}\n${current.text}` : current.text;
                saveDraft(threadId, input.value);
                autosize();
                removeOutgoing(current.submissionId);
                input.focus();
              },
            },
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

  async function deliver(item, { confirmFirst = false, fresh = false } = {}) {
    if (fresh) item.requestId = ctx.api.newRequestId();
    item.state = "sending";
    item.error = null;
    renderOutbox();
    const t = thread();
    const result = await ctx.api.sendCommand(
      command("send_chat", {
        project: item.project ?? t?.project,
        thread: threadId,
        submission_id: item.submissionId,
        composer_role: item.role,
        text: item.text,
      }),
      { requestId: item.requestId, confirmFirst },
    );
    item.requestId = result.requestId;
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
    scrollToBottom();
  }

  send.addEventListener("click", submit);
  submitShortcut(input, submit);
  input.addEventListener("input", () => saveDraft(threadId, input.value));
  roleSelect.addEventListener("change", () => {
    roleTouched = true;
  });

  // ---- store changes ----------------------------------------------------

  function onMessage(message) {
    if (message.thread !== threadId) return;
    // The committed copy of a message we sent replaces its outgoing bubble,
    // whatever the bubble's state: the commit proves the message arrived.
    if (message.role === "user" && message.content.type === "inline") {
      const items = pendingSends();
      const index = items.findIndex((item) => item.text === message.content.payload.text);
      if (index >= 0) {
        items.splice(index, 1);
        renderOutbox();
      }
    }
    if (!history) {
      // The first walk has not finished; it is applied afterwards.
      pendingMessages.push(message);
      return;
    }
    const follow = nearBottom();
    if (history.append(message)) {
      renderMessages();
      if (follow) scrollToBottom();
    }
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
          // Messages committed while disconnected are not in a snapshot.
          openHistory();
          break;
        case "receipt":
          if (change.receipt.thread === threadId) {
            if (change.receipt.state === "delivered" || change.receipt.state === "withdrawn") {
              removeOutgoing(change.receipt.submission_id);
            } else {
              renderOutbox();
            }
          }
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
      observer?.disconnect();
    },
  };

  async function start() {
    const result = await ctx.api.query("get_thread", { thread: threadId });
    if (destroyed) return;
    if (result.ok) {
      ctx.store.setThreadView(result.value);
    } else if (result.error.code === "not_found") {
      replaceChildren(el, emptyNote("このスレッドは見つかりません。"));
      return;
    } else {
      historyStatus.textContent = describeError(result.error);
    }
    const runs = await ctx.api.query("list_runs", { thread: threadId, page: pageRequest(null, 200) });
    if (runs.ok && !destroyed) ctx.store.mergePage("runs", runs.value.items);
    await openHistory();
    if (destroyed) return;
    for (const message of pendingMessages.splice(0)) onMessage(message);
  }

  renderHead();
  renderOutbox();
  start();
  return view;
}
