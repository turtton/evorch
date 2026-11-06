// Client-side read model: what the core told us, in the order it happened.
//
// Revisions are opaque (the core may use content hashes, which can go down),
// so they are never compared for order. Order comes from the event stream:
// every snapshot and every event is applied in cursor order and advances a
// local logical clock, and each entity remembers the clock value at which it
// was written. A query result is stamped with the clock value at which the
// query was issued (`mark()`); it may only write an entity that no snapshot
// or event has written since then. A query answer therefore never overwrites
// a newer event, and a query issued before a snapshot reset is ignored.

const ACTIVE_PHASES = new Set(["pending", "running", "waiting_for_user", "waiting_for_approval"]);
const KINDS = ["projects", "threads", "runs", "questions", "approvals", "receipts"];

/** Questions a lane routed to its lead: the lead answers them, not the user. */
export function isLeadRouted(question) {
  return question.target?.type === "lead";
}

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
    /** The last snapshot was cut short; lists are being completed by queries. */
    partial: false,
  };
  /** Clock value at which each entity was last written, per kind. */
  const written = Object.fromEntries(KINDS.map((kind) => [kind, new Map()]));
  let clock = 0;
  let resetAt = 0;
  const listeners = new Set();

  function notify(change) {
    for (const listener of listeners) listener(change);
  }

  function write(kind, key, item, at) {
    state[kind].set(key, item);
    written[kind].set(key, at);
  }

  /** Write a query result unless something newer already wrote the entity. */
  function writeQueried(kind, key, item, issued) {
    if (issued < resetAt) return false;
    const at = written[kind].get(key);
    if (at !== undefined && at > issued) return false;
    write(kind, key, item, issued);
    return true;
  }

  function keyOf(kind, item) {
    return kind === "receipts" ? item.submission_id : item.id;
  }

  const ITEM_KINDS = {
    project: "projects",
    thread: "threads",
    run: "runs",
    question: "questions",
    approval: "approvals",
  };

  return {
    state,

    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },

    /** The logical time to stamp on a query that is about to be sent. */
    mark() {
      return clock;
    },

    /**
     * A `subscription_started` answer. With a `snapshot_id` the snapshot
     * replaces what was held; without one the core is replaying and the
     * current state stays. A snapshot page with a `next_cursor` is partial:
     * `state.partial` stays set until `completeSnapshot()`.
     */
    applySubscriptionStarted(started) {
      const reset = Boolean(started.snapshot_id);
      if (reset) {
        clock += 1;
        resetAt = clock;
        for (const kind of KINDS) {
          if (kind === "receipts") continue;
          state[kind].clear();
          written[kind].clear();
        }
        state.threadViews.clear();
        state.partial = Boolean(started.initial_page?.next_cursor);
        for (const item of started.initial_page?.items ?? []) {
          if (item.type === "core_status") {
            state.coreStatus = item.payload;
          } else if (ITEM_KINDS[item.type]) {
            const kind = ITEM_KINDS[item.type];
            write(kind, keyOf(kind, item.payload), item.payload, clock);
          }
        }
      }
      notify({ type: reset ? "snapshot" : "replay", partial: state.partial });
      return reset;
    },

    /** The lists a partial snapshot left out have been read with queries. */
    completeSnapshot() {
      state.partial = false;
      notify({ type: "page", kind: "snapshot" });
    },

    /** Apply one UiEvent (already in cursor order). Returns the change or null. */
    applyEvent(event) {
      const p = event.payload ?? {};
      let change = null;
      switch (event.type) {
        case "core_status_changed":
          state.coreStatus = p.status;
          change = { type: "core_status" };
          break;
        case "thread_changed": {
          clock += 1;
          write("threads", p.thread.id, p.thread, clock);
          const view = state.threadViews.get(p.thread.id);
          if (view) view.summary = p.thread;
          change = { type: "thread", thread: p.thread.id, project: p.thread.project };
          break;
        }
        case "run_changed":
          clock += 1;
          write("runs", p.run.id, p.run, clock);
          change = { type: "run", thread: p.run.thread };
          break;
        case "question_changed":
          clock += 1;
          write("questions", p.question.id, p.question, clock);
          change = { type: "question", id: p.question.id, thread: p.question.requester.thread };
          break;
        case "approval_changed":
          clock += 1;
          write("approvals", p.approval.id, p.approval, clock);
          change = { type: "approval", id: p.approval.id, thread: p.approval.thread };
          break;
        case "input_receipt_changed":
          clock += 1;
          write("receipts", p.receipt.submission_id, p.receipt, clock);
          change = { type: "receipt", receipt: p.receipt };
          break;
        case "message_committed":
          change = { type: "message", message: p.message };
          break;
        default:
          return null;
      }
      notify(change);
      return change;
    },

    /**
     * Merge items from a query page issued at `issued` (from `mark()`).
     * `kind` is one of projects, threads, runs, questions, approvals, receipts.
     */
    mergePage(kind, items, issued) {
      let changed = false;
      for (const item of items) changed = writeQueried(kind, keyOf(kind, item), item, issued) || changed;
      if (changed) notify({ type: "page", kind });
      return changed;
    },

    /** A GetThread answer issued at `issued`. */
    setThreadView(view, issued) {
      if (issued < resetAt) return;
      const id = view.summary.id;
      const current = state.threads.get(id);
      const fresh = writeQueried("threads", id, view.summary, issued);
      state.threadViews.set(id, { ...view, summary: fresh ? view.summary : (current ?? view.summary) });
      notify({ type: "thread", thread: id, project: view.summary.project });
    },

    // ---- selectors ------------------------------------------------------

    /** Questions waiting for the user (lead-routed ones are the lead's). */
    pendingQuestions(threadId) {
      return [...state.questions.values()].filter(
        (q) => !q.answer && !isLeadRouted(q) && (!threadId || q.requester.thread === threadId),
      );
    },

    /** Unanswered questions routed to the lead `leadThreadId` (all leads if absent). */
    leadRoutedQuestions(leadThreadId) {
      return [...state.questions.values()].filter(
        (q) =>
          !q.answer &&
          isLeadRouted(q) &&
          (!leadThreadId || q.target.payload?.lead_thread === leadThreadId),
      );
    },

    pendingApprovals(threadId) {
      return [...state.approvals.values()].filter(
        (a) => a.state === "pending" && (!threadId || a.thread === threadId),
      );
    },

    /** Questions and approvals waiting for the user in this thread. */
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
      const time = (t) => Number(t.updated_at) || 0;
      return [...state.threads.values()]
        .filter((t) => t.project === projectId)
        .sort((a, b) => time(b) - time(a));
    },

    activeRuns(threadId) {
      return [...state.runs.values()].filter(
        (r) => r.thread === threadId && ACTIVE_PHASES.has(r.phase),
      );
    },
  };
}
