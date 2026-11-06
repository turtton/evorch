// Cards for things that wait for the user: questions and tool approvals.
// Each card keeps its own input element across updates, so text being typed
// (including an IME composition) survives a re-render.

import { h, replaceChildren } from "./dom.js";
import { label, SURFACE, shortId } from "./format.js";
import { command, describeError } from "./wire.js";

/**
 * Whether the Web may approve a tool call. ApprovalView carries only a
 * bounded `summary`, which the core may have cut short, so the person cannot
 * see the full input the tool would run with. Until the protocol can serve
 * the full input, approving is left to the desktop GUI; denying is safe and
 * stays available.
 */
export function canApproveFromWeb(_approval) {
  return false;
}

/** Lines or characters above which a summary is folded until expanded. */
const FOLD_LINES = 6;
const FOLD_CHARS = 400;

/**
 * A button that needs a second press within a short time. The first press
 * changes its label to `confirmText`; leaving it (blur) or waiting resets it.
 */
export function confirmButton({ text, confirmText, className, ariaLabel, onConfirm, resetMs = 4000 }) {
  let armed = false;
  let timer = null;
  const button = h("button", { type: "button", class: className, text, attrs: { "aria-label": ariaLabel } });
  const reset = () => {
    armed = false;
    button.textContent = text;
    button.classList.remove("armed");
    if (timer !== null) clearTimeout(timer);
    timer = null;
  };
  button.addEventListener("click", () => {
    if (!armed) {
      armed = true;
      button.textContent = confirmText;
      button.classList.add("armed");
      timer = setTimeout(reset, resetMs);
      return;
    }
    reset();
    onConfirm();
  });
  button.addEventListener("blur", reset);
  return button;
}

/** Submit on Ctrl/Cmd+Enter, never while an IME composition is open. */
export function submitShortcut(textarea, submit) {
  textarea.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" || event.isComposing || event.keyCode === 229) return;
    if (!(event.ctrlKey || event.metaKey)) return;
    event.preventDefault();
    submit();
  });
}

function threadLink(ctx, threadId) {
  return h("a", { class: "card-thread", href: `#/t/${encodeURIComponent(threadId)}`, text: ctx.threadTitle(threadId) });
}

/**
 * @param {object} ctx `{api, store, toast, threadTitle, refreshQuestions}`
 * @param {object} question a QuestionView
 * @param {{showThread?: boolean}} options
 */
export function questionCard(ctx, question, { showThread = false } = {}) {
  let q = question;
  let busy = false;
  let unknownRequest = null;

  const head = h("div", { class: "card-head" });
  const title = h("p", { class: "card-title selectable" });
  const options = h("div", { class: "options", attrs: { role: "group", "aria-label": "選択肢" } });
  const input = h("textarea", {
    class: "answer-input",
    rows: 2,
    attrs: { "aria-label": "自由入力で回答", placeholder: "自由入力で回答" },
  });
  const send = h("button", { type: "button", class: "button primary", text: "回答を送信", disabled: true });
  const status = h("p", { class: "card-status", attrs: { role: "status", "aria-live": "polite" } });
  const recheck = h("button", { type: "button", class: "button", text: "届いたか確認して再送", hidden: true });

  const el = h(
    "article",
    { class: "card question", attrs: { "aria-label": "エージェントからの質問" } },
    head,
    title,
    options,
    h("div", { class: "answer-row" }, input, send),
    status,
    recheck,
  );

  const setBusy = (value) => {
    busy = value;
    for (const button of options.querySelectorAll("button")) button.disabled = value || Boolean(q.answer);
    input.disabled = value || Boolean(q.answer);
    send.disabled = value || Boolean(q.answer) || input.value.trim() === "";
  };

  async function answer(text, { confirmFirst = false } = {}) {
    if (busy) return;
    setBusy(true);
    recheck.hidden = true;
    status.textContent = "送信中…";
    const body = command("answer_user_question", {
      thread: q.requester.thread,
      question_id: q.id,
      answer: text,
      question_revision: q.revision,
    });
    const result = await ctx.api.sendCommand(body, {
      requestId: confirmFirst ? unknownRequest.requestId : undefined,
      confirmFirst,
    });
    if (result.ok) {
      unknownRequest = null;
      input.value = "";
      status.textContent = "回答しました。";
      ctx.toast("回答を送信しました");
      ctx.refreshQuestions(q.requester.thread);
      return;
    }
    setBusy(false);
    if (result.unknown) {
      unknownRequest = { requestId: result.requestId, text };
      status.textContent = "回答が届いたか確認できませんでした。";
      recheck.hidden = false;
      return;
    }
    unknownRequest = null;
    if (result.error.code === "stale_revision") {
      status.textContent = "質問が更新されました。内容を確認してから回答し直してください。";
      ctx.refreshQuestions(q.requester.thread);
    } else if (result.error.code === "already_resolved") {
      status.textContent = "この質問は既に回答されています。";
      ctx.refreshQuestions(q.requester.thread);
    } else {
      status.textContent = describeError(result.error);
    }
  }

  input.addEventListener("input", () => setBusy(busy));
  submitShortcut(input, () => {
    if (!send.disabled) answer(input.value);
  });
  send.addEventListener("click", () => answer(input.value));
  recheck.addEventListener("click", () => {
    if (unknownRequest) answer(unknownRequest.text, { confirmFirst: true });
  });

  function render() {
    const chips = [h("span", { class: "chip", text: "質問" })];
    if (q.blocking) chips.push(h("span", { class: "chip warn", text: "実行を停止中" }));
    replaceChildren(head, chips, showThread ? threadLink(ctx, q.requester.thread) : null);
    title.textContent = q.title;
    replaceChildren(
      options,
      q.options.map((option) =>
        h("button", {
          type: "button",
          class: "button option",
          text: option,
          attrs: { "aria-label": `「${option}」と回答` },
          on: { click: () => answer(option) },
        }),
      ),
    );
    options.hidden = q.options.length === 0;
    if (q.answer) {
      const by = q.answer.answered_by ? `（${label(SURFACE, q.answer.answered_by.surface)}）` : "";
      status.textContent = `回答済み${by}: ${q.answer.text}`;
    }
    setBusy(busy);
  }

  // Re-render only on a real change, so live updates elsewhere do not
  // replace an option button that has keyboard focus.
  el.refresh = (next) => {
    const same =
      next.revision === q.revision &&
      next.management_revision === q.management_revision &&
      next.routing_revision === q.routing_revision &&
      Boolean(next.answer) === Boolean(q.answer);
    q = next;
    if (!same) render();
  };
  render();
  return el;
}

/**
 * A lane question routed to its lead (ADR 0044). The lead answers it or
 * passes it to the user, as in the desktop GUI, so it is shown read-only and
 * never counted as waiting for the user. If the lead passes it on, it comes
 * back as an ordinary question card.
 */
export function leadRoutedCard(ctx, question) {
  let q = question;
  const head = h("div", { class: "card-head" });
  const title = h("p", { class: "card-title selectable" });
  const options = h("ul", { class: "lead-options" });
  const el = h(
    "article",
    { class: "card lead-routed", attrs: { "aria-label": "Lead が対応中の質問" } },
    head,
    title,
    options,
    h("p", { class: "card-hint", text: "Lead が回答するか、ユーザーへの回答に切り替えるまで待っています。" }),
  );
  function render() {
    replaceChildren(
      head,
      h("span", { class: "chip info", text: "Lead が対応中" }),
      q.blocking ? h("span", { class: "chip warn", text: "実行を停止中" }) : null,
      threadLink(ctx, q.requester.thread),
    );
    title.textContent = q.title;
    replaceChildren(options, q.options.map((option) => h("li", { text: option })));
    options.hidden = q.options.length === 0;
  }
  el.refresh = (next) => {
    q = next;
    render();
  };
  render();
  return el;
}

function folds(text) {
  return text.length > FOLD_CHARS || text.split("\n").length > FOLD_LINES;
}

/**
 * @param {object} ctx `{api, store, toast, threadTitle, refreshApprovals}`
 * @param {object} approval an ApprovalView
 */
export function approvalCard(ctx, approval, { showThread = false } = {}) {
  let a = approval;
  let busy = false;
  let expanded = false;
  let unknownRequest = null;

  const head = h("div", { class: "card-head" });
  const summary = h("pre", { class: "approval-summary selectable" });
  const expand = h("button", { type: "button", class: "button link", attrs: { "aria-expanded": "false" } });
  const hint = h("p", { class: "card-hint", attrs: { role: "note" } });
  const status = h("p", { class: "card-status", attrs: { role: "status", "aria-live": "polite" } });
  const recheck = h("button", { type: "button", class: "button", text: "届いたか確認して再送", hidden: true });

  async function decide(decision, { confirmFirst = false } = {}) {
    if (busy) return;
    busy = true;
    render();
    recheck.hidden = true;
    status.textContent = "送信中…";
    const body = command("decide_tool_approval", {
      approval_id: a.id,
      core_epoch: a.core_epoch,
      approval_revision: a.revision,
      decision,
    });
    const result = await ctx.api.sendCommand(body, {
      requestId: confirmFirst ? unknownRequest.requestId : undefined,
      confirmFirst,
    });
    busy = false;
    if (result.ok) {
      unknownRequest = null;
      status.textContent = decision === "approve" ? "承認しました。" : "拒否しました。";
      ctx.toast(decision === "approve" ? "承認しました" : "拒否しました");
      busy = true;
      render();
      ctx.refreshApprovals(a.thread);
      return;
    }
    render();
    if (result.unknown) {
      unknownRequest = { requestId: result.requestId, decision };
      status.textContent = "決定が届いたか確認できませんでした。";
      recheck.hidden = false;
      return;
    }
    unknownRequest = null;
    const code = result.error.code;
    if (code === "stale_revision" || code === "stale_epoch") {
      status.textContent = "承認要求が更新されました。内容を確認し直してください。";
      ctx.refreshApprovals(a.thread);
    } else if (code === "already_resolved") {
      status.textContent = "この承認要求は既に決定されています。";
      ctx.refreshApprovals(a.thread);
    } else {
      status.textContent = describeError(result.error);
    }
  }

  const approve = confirmButton({
    text: "承認",
    confirmText: "もう一度押して承認",
    className: "button primary",
    ariaLabel: "このツール実行を承認",
    onConfirm: () => decide("approve"),
  });
  const deny = h("button", {
    type: "button",
    class: "button danger",
    text: "拒否",
    attrs: { "aria-label": "このツール実行を拒否" },
    on: { click: () => decide("deny") },
  });
  expand.addEventListener("click", () => {
    expanded = !expanded;
    render();
  });
  recheck.addEventListener("click", () => {
    if (unknownRequest) decide(unknownRequest.decision, { confirmFirst: true });
  });

  const el = h(
    "article",
    { class: "card approval", attrs: { "aria-label": "ツール実行の承認要求" } },
    head,
    h("p", { class: "card-label", text: "実行内容" }),
    summary,
    expand,
    hint,
    h("div", { class: "decide-row" }, deny, approve),
    status,
    recheck,
  );

  function render() {
    replaceChildren(
      head,
      h("span", { class: "chip", text: "承認待ち" }),
      h("span", { class: "chip tool", text: a.tool_name }),
      h("span", { class: "chip muted", text: `run ${shortId(a.run)}` }),
      showThread ? threadLink(ctx, a.thread) : null,
    );
    summary.textContent = a.summary;
    const foldable = folds(a.summary);
    summary.classList.toggle("folded", foldable && !expanded);
    expand.hidden = !foldable;
    expand.textContent = expanded ? "折りたたむ" : "全文を確認";
    expand.setAttribute("aria-expanded", String(expanded));
    const allowed = canApproveFromWeb(a);
    hint.hidden = allowed;
    hint.textContent = "全文を確認できないため Web からは承認できません（拒否はできます）。";
    const decided = a.state !== "pending";
    approve.disabled = busy || decided || !allowed;
    deny.disabled = busy || decided;
    if (decided && !status.textContent) {
      status.textContent = a.state === "approved" ? "承認済み" : a.state === "denied" ? "拒否済み" : "期限切れ";
    }
  }

  el.refresh = (next) => {
    const same = next.revision === a.revision && next.state === a.state;
    a = next;
    if (!same) render();
  };
  render();
  return el;
}
