// HTTP access to the core: POST /api/v1/request and GET /api/v1/health.

import {
  envelope,
  expectCommand,
  expectQuery,
  lost,
  outcomeUnknown,
  parseReply,
  query,
  randomUuid,
} from "./wire.js";

/** How many times one command is sent before the user has to decide. */
const MAX_SENDS = 3;

export class Api {
  constructor({ fetchImpl = globalThis.fetch.bind(globalThis), sessionId } = {}) {
    this.fetch = fetchImpl;
    // Groups this tab's requests; the adapter derives the client id from it.
    // It carries no privilege (ADR 0045 §5: no authentication).
    this.sessionId = sessionId ?? `s-${randomUuid()}`;
  }

  newRequestId() {
    return `req-${randomUuid()}`;
  }

  /** Send one request. Never throws: failures are `{ok: false, error}`. */
  async request(body, requestId = this.newRequestId()) {
    let response;
    try {
      response = await this.fetch("/api/v1/request", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(envelope(requestId, this.sessionId, body)),
        credentials: "same-origin",
        cache: "no-store",
      });
    } catch {
      return lost("network error");
    }
    let json;
    try {
      json = await response.json();
    } catch {
      return lost(`unreadable answer (HTTP ${response.status})`);
    }
    return parseReply(json);
  }

  async query(type, payload) {
    return expectQuery(await this.request(query(type, payload)), queryReplyType(type));
  }

  async health() {
    let response;
    try {
      response = await this.fetch("/api/v1/health", { cache: "no-store", credentials: "same-origin" });
    } catch {
      return lost("network error");
    }
    let json;
    try {
      json = await response.json();
    } catch {
      return lost(`unreadable answer (HTTP ${response.status})`);
    }
    if (json && json.status === "ok" && json.hello) return { ok: true, value: json };
    const parsed = parseReply(json);
    return parsed.ok ? lost("unexpected health answer") : parsed;
  }

  /** What the core recorded for an earlier request of this session. */
  async commandReceipt(requestId) {
    return this.query("get_command_receipt", { request_id: requestId });
  }

  /**
   * Send a command so that it takes effect at most once.
   *
   * The request id stays the same across attempts, and a SendChat keeps its
   * submission id inside `body`, so the core can recognise a repeat. When an
   * attempt ends without a decided answer (network failure, internal error),
   * the core is asked first whether it applied the request; it is sent again
   * only when the core has no record of it.
   *
   * With `confirmFirst` the receipt is checked before the first send (used
   * when the user retries a command whose outcome was unknown).
   *
   * Resolves `{ok: true, value: CommandReply}`, `{ok: false, error}` for a
   * decided refusal, or `{ok: false, error, unknown: true}` when the outcome
   * is still unknown; then the same `requestId` must be used to try again.
   */
  async sendCommand(body, { requestId = this.newRequestId(), confirmFirst = false } = {}) {
    let lastError = null;
    for (let attempt = 0; attempt < MAX_SENDS; attempt += 1) {
      if (confirmFirst || attempt > 0) {
        const receipt = await this.commandReceipt(requestId);
        if (!receipt.ok) {
          return { ok: false, error: receipt.error, unknown: true, requestId };
        }
        const state = receipt.value.state;
        if (state.type === "applied") return { ok: true, value: state.payload.reply, requestId };
        if (state.type === "rejected") return { ok: false, error: state.payload.error, requestId };
      }
      const result = expectCommand(await this.request(body, requestId));
      if (result.ok) return { ...result, requestId };
      if (!outcomeUnknown(result.error)) return { ...result, requestId };
      lastError = result.error;
    }
    return { ok: false, error: lastError, unknown: true, requestId };
  }
}

/** The QueryReply variant each query answers with. */
export function queryReplyType(type) {
  return QUERY_REPLY[type] ?? type;
}

const QUERY_REPLY = {
  get_core_status: "core_status",
  list_projects: "projects",
  list_threads: "threads",
  get_thread: "thread",
  list_runs: "runs",
  list_questions: "questions",
  list_approvals: "approvals",
  get_input_receipts: "input_receipts",
  get_command_receipt: "command_receipt",
  read_committed_history: "committed_history",
  read_body: "body",
};
