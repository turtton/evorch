// Management of the core from the browser: restart, quarantine and owned graphs.
//
// This module is a client of operations that the server only allows for a
// verified principal. In production the web adapter has no verifier yet, so
// every one of them is refused with `permission_denied`; the view then stays
// in its disabled state. Nothing here protects anything by being hidden: the
// server decides, and this code only never sends what it was not shown.
//
// Rules this module keeps (the plan's DC-30..40):
// - A target is only ever named by the id the core listed. No path, argv or
//   environment exists in any request built here.
// - A confirmation captures the binding the user was shown. When a refresh
//   shows something else (another revision or core epoch), the confirmation
//   is void and the user has to confirm again.
// - A mutation is sent once. An outcome that is not known is read back with
//   the same operation id (OperationStatus); it is never sent again blindly.
// - After a new core epoch every held binding and cursor is dropped.

import { replaceChildren, h } from "./dom.js";
import { EventStream } from "./sse.js";
import { describeError, randomUuid } from "./wire.js";

// ---- request bodies ---------------------------------------------------------

function lifecycle(kind, type, inner) {
  const payload = inner === undefined ? { type } : { type, payload: inner };
  return { kind, payload: { type: "lifecycle", payload } };
}

/** `{type: group, payload: {type: operation, payload?}}` of a lifecycle operation. */
function nested(kind, group, type, payload) {
  const inner = payload === undefined ? { type } : { type, payload };
  return lifecycle(kind, group, inner);
}

export const adminQueries = {
  targets: () => nested("query", "restart", "targets"),
  status: () => nested("query", "restart", "status"),
  quarantineList: () => nested("query", "quarantine", "list"),
  undisposed: () => nested("query", "owned_graphs", "undisposed"),
  ownedDetail: (graphRef) => nested("query", "owned_graphs", "detail", { graph_ref: graphRef }),
  operationStatus: (operationId) => lifecycle("query", "operation_status", { operation_id: operationId }),
};

export function newOperationId() {
  return `op-${randomUuid()}`;
}

export const adminCommands = {
  /** The binding is exactly what the user was shown and confirmed. */
  requestRestart: (target, coreEpoch, operationId) =>
    nested("command", "restart", "request", {
      binding: {
        target_id: target.id,
        expected_core_epoch: coreEpoch,
        target_revision: target.revision,
        operation_id: operationId,
      },
      confirmed: true,
    }),
  cancelRestart: (status, coreEpoch, operationId) =>
    nested("command", "restart", "cancel", {
      restart_id: status.restart_id,
      expected_core_epoch: coreEpoch,
      expected_revision: String(status.revision),
      operation_id: operationId,
    }),
  interrupt: (binding, operationId) =>
    nested("command", "restart", "interrupt", { binding, operation_id: operationId, confirmed: true }),
  discard: (binding, operationId) =>
    nested("command", "quarantine", "discard", { binding: { ...binding, operation_id: operationId }, confirmed: true }),
  release: (binding, operationId) =>
    nested("command", "owned_graphs", "release", {
      binding: withOperation(binding, operationId),
      confirmed: true,
    }),
  resume: (binding, operationId) =>
    nested("command", "owned_graphs", "resume", {
      binding: withOperation(binding, operationId),
      confirmed: true,
    }),
};

function withOperation(binding, operationId) {
  return { ...binding, graph: { ...binding.graph, operation_id: operationId } };
}

// ---- replies ----------------------------------------------------------------

/**
 * The value of a successful lifecycle query: the lifecycle reply
 * `{type: group, payload: {type, payload}}` is unwrapped to the inner payload
 * when `group` and `type` match (`group` null for a reply with no group).
 */
export function unwrapAdminQuery(result, group, type) {
  if (!result.ok) return result;
  if (result.kind !== "query") return unexpected(type);
  const reply = result.value?.type === "lifecycle" ? result.value.payload : null;
  if (group === null) {
    return reply?.type === type ? { ok: true, value: reply.payload } : unexpected(type);
  }
  const inner = reply?.type === group ? reply.payload : null;
  return inner?.type === type ? { ok: true, value: inner.payload } : unexpected(type);
}

/** The AdminReceipt of an accepted lifecycle command. */
export function unwrapReceipt(result) {
  if (!result.ok) return result;
  const answer = result.value?.result;
  if (result.kind !== "command" || answer?.type !== "lifecycle" || answer.payload?.type !== "accepted") {
    return unexpected("accepted receipt");
  }
  return { ok: true, value: answer.payload.payload };
}

function unexpected(what) {
  return { ok: false, error: { code: "internal", message: `expected ${what}`, retry: "never" } };
}

/** Errors that mean "what you confirmed is not what is there now". */
export function isConflict(error) {
  return ["conflict", "stale_revision", "stale_epoch", "already_resolved"].includes(error?.code);
}

export function isDenied(error) {
  return error?.code === "permission_denied";
}

// ---- the model --------------------------------------------------------------

/** Restart phases in which the core still accepts a cancel. */
const CANCELLABLE = new Set(["requested", "preparing_drains", "waiting_safe_point", "sealing"]);

export function isCancellable(status) {
  return Boolean(status?.restart_id) && CANCELLABLE.has(status.phase);
}

/** A blocker that carries everything an Interrupt is bound to; none does today. */
export function interruptBindingOf(status, blocker) {
  const needed = ["blocker_identity", "run_incarnation", "connection_epoch", "core_epoch"];
  if (!status?.restart_id || needed.some((name) => !blocker?.[name])) return null;
  return {
    restart_id: status.restart_id,
    core_epoch: blocker.core_epoch,
    connection_epoch: blocker.connection_epoch,
    run_incarnation: blocker.run_incarnation,
    blocker_identity: blocker.blocker_identity,
    revision: String(status.revision),
    phase: status.phase,
  };
}

export function createAdminModel() {
  const state = {
    /** `unknown` until probed; `enabled` only after the core answered a query. */
    access: "unknown",
    coreEpoch: null,
    targetsRevision: null,
    targets: new Map(),
    status: null,
    quarantine: new Map(),
    undisposed: null,
    details: new Map(),
    /** The open confirmation, or null. */
    confirmation: null,
  };
  const listeners = new Set();
  const notify = () => listeners.forEach((listener) => listener());

  function clearHeld() {
    state.targetsRevision = null;
    state.targets.clear();
    state.status = null;
    state.quarantine.clear();
    state.undisposed = null;
    state.details.clear();
  }

  /** What `confirmation` was bound to, as it stands in the latest data. */
  function fingerprintOf(kind, key) {
    switch (kind) {
      case "restart": {
        const target = state.targets.get(key);
        return target && state.coreEpoch !== null ? `${state.coreEpoch}|${target.revision}` : null;
      }
      case "discard": {
        const item = state.quarantine.get(key);
        return item?.binding ? JSON.stringify([item.revision, item.binding]) : null;
      }
      case "release":
      case "resume": {
        const view = state.details.get(key);
        return view ? JSON.stringify([view.snapshot_revision, view.binding]) : null;
      }
      case "cancel":
      case "interrupt":
        return state.status ? JSON.stringify([state.status.restart_id, state.status.revision]) : null;
      default:
        return null;
    }
  }

  return {
    state,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    fingerprintOf,

    setAccess(access) {
      if (state.access === access) return;
      state.access = access;
      if (access !== "enabled") {
        clearHeld();
        state.confirmation = null;
      }
      notify();
    },

    /** Drop everything held, as after a new core epoch or a fresh snapshot. */
    reset() {
      clearHeld();
      if (state.confirmation) state.confirmation.void = "reset";
      notify();
    },

    /** The management epoch changed: nothing held for the old one means anything. */
    observeCoreEpoch(epoch) {
      if (state.coreEpoch === epoch) return false;
      const changed = state.coreEpoch !== null;
      state.coreEpoch = epoch;
      if (changed) this.reset();
      return changed;
    },

    applyTargets(reply) {
      this.observeCoreEpoch(reply.core_epoch);
      state.targets = new Map(reply.targets.map((target) => [target.id, target]));
      state.targetsRevision = reply.revision;
      state.targetsTruncated = reply.truncated;
      notify();
    },
    applyStatus(reply) {
      this.observeCoreEpoch(reply.core_epoch);
      state.status = { ...reply.status, audit: reply.audit };
      notify();
    },
    applyQuarantine(reply) {
      state.quarantine = new Map(reply.items.map((item) => [item.graph_id, item]));
      state.quarantineTruncated = reply.truncated;
      notify();
    },
    applyUndisposed(reply) {
      state.undisposed = reply;
      notify();
    },
    applyDetail(view) {
      state.details.set(view.graph_ref, view);
      notify();
    },

    /**
     * Open a confirmation. `build(operationId)` makes the request from the
     * binding captured now; the data it was bound to is fingerprinted.
     */
    openConfirmation({ kind, key, title, lines, build }) {
      state.confirmation = {
        kind,
        key,
        title,
        lines,
        build,
        fingerprint: fingerprintOf(kind, key),
        operationId: newOperationId(),
        sending: false,
        sent: false,
        void: null,
        outcome: null,
      };
      notify();
      return state.confirmation;
    },
    closeConfirmation() {
      state.confirmation = null;
      notify();
    },
    /** Why the open confirmation cannot be sent, or null when it can. */
    confirmationProblem() {
      const c = state.confirmation;
      if (!c) return "none";
      if (c.void) return "void";
      if (c.sent) return c.sending ? "sending" : "sent";
      if (c.fingerprint === null || fingerprintOf(c.kind, c.key) !== c.fingerprint) return "stale";
      return null;
    },
    /**
     * Record that `c` is being sent. Once sent it can never be sent again: an
     * answer that was lost leaves the outcome unknown, and only reading the
     * same operation id back is allowed from then on. Ignored when `c` is no
     * longer the open confirmation.
     */
    markSending(c, sending) {
      if (state.confirmation !== c) return;
      c.sending = sending;
      if (sending) c.sent = true;
      notify();
    },
    /** Show the outcome on `c` only while `c` is still the open confirmation. */
    setOutcome(c, outcome) {
      if (state.confirmation !== c) return false;
      c.outcome = outcome;
      notify();
      return true;
    },
  };
}

// ---- the controller ---------------------------------------------------------

const ACCESS_TEXT = {
  unknown: "管理の可否を確認しています…",
  disabled:
    "Web からの管理操作は無効です。認証済みの主体がないため、コアはすべての管理操作を拒否します。デスクトップまたは管理 CLI を使ってください。",
  unavailable: "管理情報を取得できませんでした。",
  enabled: "",
};

const PHASE_TEXT = {
  running: "通常稼働",
  requested: "要求済み",
  preparing_drains: "受付を止めています",
  waiting_safe_point: "安全点を待っています",
  sealing: "封印中",
  sealed: "封印済み",
  capturing: "状態を取得中",
  finalizing: "最終処理中",
  cancelling: "取消中",
  failed_closed: "失敗（閉塞）",
  shutting_down: "停止処理中",
};

const BLOCKER_TEXT = {
  in_flight_admission: "受付処理中",
  accepted_resume_pending: "再開待ち",
  awaiting_safe_point: "安全点待ち",
  shell_running: "シェル実行中",
  shell_result_unobserved: "シェル結果が未観測",
  unconfirmed_run_js: "run_js が未確認",
  unfinished_tool_batch: "ツール実行が未完了",
};

/**
 * The management screen. `ctx.api` sends the requests; `streamFactory` makes
 * the management EventStream (replaceable in tests).
 */
export function adminView(ctx, { streamFactory = (options) => new EventStream(options) } = {}) {
  const model = createAdminModel();
  const { state } = model;
  const api = ctx.api;
  let stream = null;
  let destroyed = false;
  let notice = "";

  const accessNote = h("p", { class: "empty admin-access", attrs: { role: "status" } });
  const body = h("div", { class: "admin-body" });
  const el = h(
    "section",
    { class: "screen admin", attrs: { "aria-labelledby": "admin-heading" } },
    h("h2", { id: "admin-heading", class: "section-title", text: "管理" }),
    accessNote,
    body,
  );

  // ---- requests ----

  async function query(bodyOf, group, type) {
    const value = unwrapAdminQuery(await api.adminRequest(bodyOf), group, type);
    if (!value.ok && isDenied(value.error)) model.setAccess("disabled");
    return value;
  }

  async function refreshTargets() {
    const reply = await query(adminQueries.targets(), "restart", "targets");
    if (reply.ok) model.applyTargets(reply.value);
    return reply;
  }
  async function refreshStatus() {
    const reply = await query(adminQueries.status(), "restart", "status");
    if (reply.ok) model.applyStatus(reply.value);
    return reply;
  }
  async function refreshQuarantine() {
    const reply = await query(adminQueries.quarantineList(), "quarantine", "list");
    if (reply.ok) model.applyQuarantine(reply.value);
    return reply;
  }
  async function refreshUndisposed() {
    const reply = await query(adminQueries.undisposed(), "owned_graphs", "undisposed");
    if (reply.ok) model.applyUndisposed(reply.value);
    return reply;
  }
  async function refreshDetail(graphRef) {
    const reply = await query(adminQueries.ownedDetail(graphRef), "owned_graphs", "detail");
    if (reply.ok) model.applyDetail(reply.value);
    return reply;
  }
  async function refreshAll() {
    // Targets first: they carry the management epoch the other answers are read against.
    const first = await refreshTargets();
    if (!first.ok) return first;
    await Promise.all([refreshStatus(), refreshQuarantine(), refreshUndisposed()]);
    return first;
  }

  /** Probe once; only a decided answer from the core enables the actions. */
  async function probe() {
    const first = await refreshTargets();
    if (destroyed) return;
    if (first.ok) {
      model.setAccess("enabled");
      await Promise.all([refreshStatus(), refreshQuarantine(), refreshUndisposed()]);
      startStream();
    } else if (isDenied(first.error)) {
      model.setAccess("disabled");
    } else {
      notice = describeError(first.error);
      model.setAccess("unavailable");
    }
    render();
  }

  // ---- the management stream ----

  function startStream() {
    if (stream || destroyed) return;
    stream = streamFactory({
      scope: "management",
      onStarted(started) {
        // A snapshot means the cursor was not honoured (a new epoch or too
        // old): nothing held before it can be trusted.
        if (started.snapshot_id) {
          model.reset();
          refreshAll();
        }
      },
      onEvent(envelope) {
        if (envelope.event.type === "admin_changed") {
          refreshStatus();
          refreshTargets();
        }
      },
      async onStatus(status) {
        el.dataset.stream = status.state;
        // Reconnecting asks the core again whether this page may still manage:
        // a lost or expired authorization ends the stream and the view.
        if (status.state !== "waiting" || destroyed) return;
        const reply = await refreshTargets();
        if (!reply.ok && isDenied(reply.error)) {
          stream?.stop();
          stream = null;
        }
      },
    });
    stream.start();
  }

  // ---- actions ----

  function ask(spec) {
    model.openConfirmation(spec);
  }

  async function send() {
    const c = state.confirmation;
    if (!c || model.confirmationProblem() !== null) return;
    model.markSending(c, true);
    const result = await api.adminRequest(c.build(c.operationId));
    if (destroyed) return;
    model.markSending(c, false);
    const receipt = unwrapReceipt(result);
    if (receipt.ok) {
      model.setOutcome(c, { type: outcomeType(receipt.value), receipt: receipt.value });
      refreshStatus();
      refreshQuarantine();
      refreshUndisposed();
      return;
    }
    if (isConflict(receipt.error)) {
      model.setOutcome(c, { type: "conflict", error: receipt.error });
      await refreshAll();
      return;
    }
    if (isDenied(receipt.error)) {
      model.setOutcome(c, { type: "denied", error: receipt.error });
      model.setAccess("disabled");
      return;
    }
    if (receipt.error.code === "transport" || receipt.error.code === "internal" || receipt.error.code === "storage_unavailable") {
      model.setOutcome(c, { type: "unknown", error: receipt.error });
      return;
    }
    model.setOutcome(c, { type: "failed", error: receipt.error });
  }

  function outcomeType(receipt) {
    if (receipt.state === "unknown") return "unknown";
    switch (receipt.outcome) {
      case "succeeded":
        return "accepted";
      case "conflict":
        return "conflict";
      case "failed":
      case "cancelled":
        return "failed";
      default:
        return "pending";
    }
  }

  async function readBack() {
    const c = state.confirmation;
    if (!c) return;
    const receipt = unwrapAdminQuery(
      await api.adminRequest(adminQueries.operationStatus(c.operationId)),
      null,
      "operation_status",
    );
    if (receipt.ok) {
      model.setOutcome(c, { type: outcomeType(receipt.value), receipt: receipt.value });
      refreshStatus();
    } else {
      model.setOutcome(c, { type: "unknown", error: receipt.error });
    }
  }

  // ---- rendering ----

  function button(text, onClick, { disabled = false, cls = "button small", reason } = {}) {
    return h("button", {
      type: "button",
      class: cls,
      text,
      disabled,
      attrs: { "aria-disabled": disabled || null, title: disabled ? reason : null },
      on: { click: onClick },
    });
  }

  function targetsSection(enabled) {
    const rows = [...state.targets.values()].map((target) =>
      h(
        "li",
        { class: "row-item", dataset: { target: target.id } },
        h(
          "div",
          { class: "row" },
          h(
            "span",
            { class: "row-main" },
            h("span", { class: "row-title", text: target.id }),
            h("span", { class: "row-sub", text: `リビジョン ${target.revision}` }),
          ),
          button(
            "再起動を要求",
            () =>
              ask({
                kind: "restart",
                key: target.id,
                title: `コアを「${target.id}」へ再起動します`,
                lines: [
                  `対象: ${target.id}（リビジョン ${target.revision}）`,
                  `コア世代: ${state.coreEpoch}`,
                  "実行中の作業は安全点まで待ってから切り替わります。実行は取り消せないため、確認が必要です。",
                ],
                build: ((epoch, snapshot) => (operationId) => adminCommands.requestRestart(snapshot, epoch, operationId))(
                  state.coreEpoch,
                  { id: target.id, revision: target.revision },
                ),
              }),
            { disabled: !enabled, reason: "管理操作は無効です" },
          ),
        ),
      ),
    );
    return h(
      "div",
      { class: "admin-section", dataset: { section: "targets" } },
      h("h3", { class: "section-title", text: "再起動先" }),
      rows.length
        ? h("ul", { class: "rows", attrs: { "aria-label": "再起動先" } }, rows)
        : h("p", { class: "empty", text: enabled ? "再起動先はありません。" : "（無効）" }),
    );
  }

  function statusSection(enabled) {
    const status = state.status;
    const children = [h("h3", { class: "section-title", text: "再起動の状態" })];
    if (!status) {
      children.push(h("p", { class: "empty", text: enabled ? "状態を読み込めていません。" : "（無効）" }));
    } else {
      children.push(
        h("p", { class: "admin-phase", text: `${PHASE_TEXT[status.phase] ?? status.phase}（リビジョン ${status.revision}）` }),
      );
      if (status.blocker_total > 0) {
        const blockers = status.blockers.map((b) => h("li", { text: `${BLOCKER_TEXT[b.kind] ?? b.kind}（run ${b.run}）` }));
        children.push(
          h("p", { class: "list-status", text: `待機中の要因 ${status.blocker_total} 件` }),
          h("ul", { class: "admin-blockers" }, blockers),
          h("p", {
            class: "card-hint",
            text: "シェルへの割り込みは、コアが確認対象（blocker・run の識別）を返すまで Web からは行えません。",
          }),
        );
      }
      if (Number(status.audit?.denial_persistence_failures ?? 0) > 0) {
        children.push(h("p", { class: "list-status", text: "拒否の監査記録に失敗しています。" }));
      }
    }
    children.push(
      button(
        "再起動を取り消す",
        () => cancelNow(),
        { disabled: !enabled || !isCancellable(status), cls: "button small danger", reason: "取消できる状態ではありません" },
      ),
    );
    return h("div", { class: "admin-section", dataset: { section: "status" } }, children);
  }

  async function cancelNow() {
    if (!isCancellable(state.status)) return;
    ask({
      kind: "cancel",
      key: state.status.restart_id,
      title: "再起動の要求を取り消します",
      lines: [`再起動 ${state.status.restart_id}（リビジョン ${state.status.revision}）`],
      build: ((status, epoch) => (operationId) => adminCommands.cancelRestart(status, epoch, operationId))(
        { ...state.status },
        state.coreEpoch,
      ),
    });
  }

  function quarantineSection(enabled) {
    const items = [...state.quarantine.values()].map((item) => {
      const canDiscard = enabled && Boolean(item.binding);
      return h(
        "li",
        { class: "row-item", dataset: { graph: item.graph_id } },
        h(
          "div",
          { class: "row" },
          h(
            "span",
            { class: "row-main" },
            h("span", { class: "row-title", text: item.graph_id }),
            h("span", { class: "row-sub", text: `${item.cold ? "コールド隔離" : "隔離"}・リビジョン ${item.revision}` }),
          ),
          button(
            "再開資格を失効",
            () =>
              ask({
                kind: "discard",
                key: item.graph_id,
                title: `隔離された graph「${item.graph_id}」を処分します`,
                lines: ["再開資格が恒久的に失効します。履歴は書き換わりません。"],
                build: ((binding) => (operationId) => adminCommands.discard(binding, operationId))({ ...item.binding }),
              }),
            { disabled: !canDiscard, reason: "コアが確認情報を返していないため Web からは処分できません" },
          ),
        ),
      );
    });
    return h(
      "div",
      { class: "admin-section", dataset: { section: "quarantine" } },
      h("h3", { class: "section-title", text: "隔離された graph" }),
      items.length
        ? h("ul", { class: "rows", attrs: { "aria-label": "隔離された graph" } }, items)
        : h("p", { class: "empty", text: enabled ? "隔離された graph はありません。" : "（無効）" }),
    );
  }

  function ownedSection(enabled) {
    const list = state.undisposed;
    const rows = (list?.references ?? []).map((ref) => {
      const view = state.details.get(ref.graph_id);
      const available = (action) => view?.[action]?.type === "available";
      const act = (kind, verb) =>
        ask({
          kind,
          key: ref.graph_id,
          title: `graph「${ref.graph_id}」を${verb}します`,
          lines: [
            `リビジョン ${view.snapshot_revision}・所有者 ${view.owner_count} 件`,
            kind === "release" ? "所有権を解除します（再開はしません）。" : "明示的に再開します。",
          ],
          build: ((binding) => (operationId) => adminCommands[kind](binding, operationId))(structuredClone(view.binding)),
        });
      return h(
        "li",
        { class: "row-item", dataset: { graph: ref.graph_id } },
        h(
          "div",
          { class: "row" },
          h(
            "span",
            { class: "row-main" },
            h("span", { class: "row-title", text: ref.graph_id }),
            h("span", { class: "row-sub", text: view ? `${view.eligibility}・${view.ownership}` : `リビジョン ${ref.revision}` }),
          ),
          button("詳細を読む", () => refreshDetail(ref.graph_id), { disabled: !enabled }),
          button("所有権を解除", () => act("release", "解除"), {
            disabled: !enabled || !available("release"),
            reason: "詳細を読み、解除できる状態のときだけ使えます",
          }),
          button("明示的に再開", () => act("resume", "再開"), {
            disabled: !enabled || !available("resume"),
            reason: "詳細を読み、再開できる状態のときだけ使えます",
          }),
        ),
      );
    });
    return h(
      "div",
      { class: "admin-section", dataset: { section: "owned" } },
      h("h3", { class: "section-title", text: "未処分の graph" }),
      list && rows.length
        ? h("ul", { class: "rows", attrs: { "aria-label": "未処分の graph" } }, rows)
        : h("p", { class: "empty", text: enabled ? "未処分の graph はありません。" : "（無効）" }),
      list?.truncated ? h("p", { class: "list-status", text: `ほか ${Number(list.count) - rows.length} 件は表示していません。` }) : null,
    );
  }

  const OUTCOME_TEXT = {
    accepted: "受け付けました。",
    pending: "処理中です。状態を再確認してください。",
    conflict: "確認した対象が変わっていました。最新の内容を読み直したので、もう一度確認してください。",
    denied: "拒否されました。認証が必要です。",
    unknown: "結果を確認できませんでした。同じ操作 ID で状態を確認できます（再送はしません）。",
    failed: "失敗しました。",
  };

  function confirmationPanel() {
    const c = state.confirmation;
    if (!c) return null;
    const problem = model.confirmationProblem();
    const outcome = c.outcome;
    const parts = [h("strong", { text: c.title }), ...c.lines.map((line) => h("p", { class: "card-hint", text: line }))];
    if (c.void || problem === "stale") {
      parts.push(h("p", { class: "list-status admin-void", text: "確認した対象が変わりました。この確認は無効です。最新の内容で確認し直してください。" }));
    }
    if (outcome) {
      let text = OUTCOME_TEXT[outcome.type] ?? "";
      if (outcome.error) text += `（${describeError(outcome.error)}）`;
      parts.push(h("p", { class: "list-status admin-outcome", dataset: { outcome: outcome.type }, text }));
      if (outcome.receipt?.audit_warning) {
        parts.push(h("p", { class: "list-status admin-audit-warning", text: "監査の記録に失敗しました。操作自体は受け付けられています。" }));
      }
    }
    const done = outcome && outcome.type !== "unknown" && outcome.type !== "pending";
    // A sent operation is never offered again: only its state can be read back.
    const readBackOnly = c.sent && !done;
    parts.push(
      h(
        "div",
        { class: "decide-row" },
        c.sent
          ? null
          : button("実行する", send, {
              disabled: problem !== null,
              cls: "button small danger",
              reason: "この確認はいま実行できません",
            }),
        c.sending ? h("span", { class: "list-status", text: "送信中…" }) : null,
        outcome && !done ? button("状態を確認", readBack, { cls: "button small readback" }) : null,
        c.sending ? null : button(done || readBackOnly ? "閉じる" : "やめる", () => model.closeConfirmation()),
      ),
    );
    return h("div", { class: "card admin-confirm", dataset: { kind: c.kind }, attrs: { role: "group", "aria-label": "確認" } }, parts);
  }

  function render() {
    if (destroyed) return;
    const enabled = state.access === "enabled";
    el.dataset.access = state.access;
    const text = state.access === "unavailable" ? `${ACCESS_TEXT.unavailable}（${notice}）` : ACCESS_TEXT[state.access];
    accessNote.textContent = text;
    accessNote.hidden = text === "";
    if (state.access === "unknown") {
      replaceChildren(body);
      return;
    }
    replaceChildren(
      body,
      confirmationPanel(),
      targetsSection(enabled),
      statusSection(enabled),
      quarantineSection(enabled),
      ownedSection(enabled),
    );
  }

  const unsubscribe = model.subscribe(render);
  render();
  probe();

  return {
    el,
    title: "管理",
    back: "#/",
    model,
    onChange() {},
    destroy() {
      destroyed = true;
      unsubscribe();
      stream?.stop();
      stream = null;
    },
  };
}
