// Client-side read model: what the core told us, kept idempotent.
//
// Snapshot items, query results and UI events all go through upsert(), which
// keeps an entity only if its revision is not older than the one already
// held. Replaying an event twice, or applying a query page cut before an
// event that already arrived, therefore changes nothing.

import { compareDecimal } from "./wire.js";

const ACTIVE_PHASES = new Set(["pending", "running", "waiting_for_user", "waiting_for_approval"]);

/** Compare entity versions: revision first, then any tie breakers. */
function compareVersions(a, b, keys) {
  for (const key of keys) {
    const order = compareDecimal(a[key] ?? "0", b[key] ?? "0");
    if (order !== 0) return order;
  }
  return 0;
}

const VERSION_KEYS = {
  projects: ["revision"],
  threads: ["revision"],
  runs: ["revision"],
  questions: ["revision", "management_revision", "routing_revision"],
  approvals: ["revision"],
  receipts: ["revision"],
};

export function createStore() {
  const state = {
    coreStatus: null,
    projects: new Map(),
    threads: new Map(),
    runs: new Map(),
    questions: new Map(),
    approvals: new Map(),
    receipts: new Map(),
    threadViews: new Map(),
  };
  const listeners = new Set();

  function notify(change) {
    for (const listener of listeners) listener(change);
  }

  /** Returns true when the item was stored (newer or equal revision). */
  function upsert(kind, key, item) {
    const map = state[kind];
    const held = map.get(key);
    if (held && compareVersions(item, held, VERSION_KEYS[kind]) < 0) return false;
    map.set(key, item);
    return true;
  }

  function applyItem(type, item) {
    switch (type) {
      case "core_status":
        state.coreStatus = item;
        return true;
      case "project":
        return upsert("projects", item.id, item);
      case "thread":
        return upsert("threads", item.id, item);
      case "run":
        return upsert("runs", item.id, item);
      case "question":
        return upsert("questions", item.id, item);
      case "approval":
        return upsert("approvals", item.id, item);
      default:
        return false;
    }
  }

  return {
    state,

    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },

    /**
     * A `subscription_started` answer. With a `snapshot_id` the snapshot
     * replaces what was held (items absent from it are gone); without one the
     * core is replaying and the current state stays.
     */
    applySubscriptionStarted(started) {
      const reset = Boolean(started.snapshot_id);
      if (reset) {
        state.projects.clear();
        state.threads.clear();
        state.runs.clear();
        state.questions.clear();
        state.approvals.clear();
      }
      for (const item of started.initial_page?.items ?? []) applyItem(item.type, item.payload);
      notify({ type: reset ? "snapshot" : "replay" });
      return reset;
    },

    /** Apply one UiEvent. Returns the change, or null if nothing changed. */
    applyEvent(event) {
      const p = event.payload ?? {};
      let change = null;
      switch (event.type) {
        case "core_status_changed":
          state.coreStatus = p.status;
          change = { type: "core_status" };
          break;
        case "thread_changed":
          if (upsert("threads", p.thread.id, p.thread)) {
            const view = state.threadViews.get(p.thread.id);
            if (view && compareDecimal(p.thread.revision, view.summary.revision) >= 0) {
              view.summary = p.thread;
            }
            change = { type: "thread", thread: p.thread.id, project: p.thread.project };
          }
          break;
        case "run_changed":
          if (upsert("runs", p.run.id, p.run)) change = { type: "run", thread: p.run.thread };
          break;
        case "question_changed":
          if (upsert("questions", p.question.id, p.question)) {
            change = { type: "question", id: p.question.id, thread: p.question.requester.thread };
          }
          break;
        case "approval_changed":
          if (upsert("approvals", p.approval.id, p.approval)) {
            change = { type: "approval", id: p.approval.id, thread: p.approval.thread };
          }
          break;
        case "input_receipt_changed":
          if (upsert("receipts", p.receipt.submission_id, p.receipt)) {
            change = { type: "receipt", receipt: p.receipt };
          }
          break;
        case "message_committed":
          change = { type: "message", message: p.message };
          break;
        default:
          return null;
      }
      if (change) notify(change);
      return change;
    },

    /** Merge items from a query page (`kind` is a key of VERSION_KEYS). */
    mergePage(kind, items) {
      let changed = false;
      for (const item of items) {
        const key = kind === "receipts" ? item.submission_id : item.id;
        changed = upsert(kind, key, item) || changed;
      }
      if (changed) notify({ type: "page", kind });
      return changed;
    },

    setThreadView(view) {
      state.threadViews.set(view.summary.id, view);
      upsert("threads", view.summary.id, view.summary);
      notify({ type: "thread", thread: view.summary.id, project: view.summary.project });
    },

    // ---- selectors ------------------------------------------------------

    pendingQuestions(threadId) {
      return [...state.questions.values()].filter(
        (q) => !q.answer && (!threadId || q.requester.thread === threadId),
      );
    },

    pendingApprovals(threadId) {
      return [...state.approvals.values()].filter(
        (a) => a.state === "pending" && (!threadId || a.thread === threadId),
      );
    },

    /** Questions and approvals waiting for someone in this thread. */
    threadAttention(threadId) {
      return {
        questions: this.pendingQuestions(threadId).length,
        approvals: this.pendingApprovals(threadId).length,
      };
    },

    projectAttention(projectId) {
      let questions = 0;
      let approvals = 0;
      for (const q of this.pendingQuestions()) {
        if (state.threads.get(q.requester.thread)?.project === projectId) questions += 1;
      }
      for (const a of this.pendingApprovals()) {
        if (state.threads.get(a.thread)?.project === projectId) approvals += 1;
      }
      return { questions, approvals };
    },

    totalAttention() {
      return {
        questions: this.pendingQuestions().length,
        approvals: this.pendingApprovals().length,
      };
    },

    threadsOf(projectId) {
      return [...state.threads.values()]
        .filter((t) => t.project === projectId)
        .sort((a, b) => compareDecimal(b.updated_at, a.updated_at));
    },

    activeRuns(threadId) {
      return [...state.runs.values()].filter(
        (r) => r.thread === threadId && ACTIVE_PHASES.has(r.phase),
      );
    },

    runsOf(threadId) {
      return [...state.runs.values()].filter((r) => r.thread === threadId);
    },
  };
}
