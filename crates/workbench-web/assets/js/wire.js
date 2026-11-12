// Wire helpers for the workbench protocol (ADR 0045 §4).
//
// Pure functions only: no DOM, no network. The shapes follow
// crates/workbench-protocol (golden fixtures in its tests/golden):
//   request  {request_id, session_id, kind: "command"|"query",
//            payload: {type: <namespace>, payload: {type: <operation>, payload?}}}
//   reply    {request_id?, status: "success"|"error", payload}
//   event    {type: <namespace>, payload: {type: <event>, payload?}}
// Callers name only the operation; the namespace each operation belongs to is
// the table below (the protocol's `namespaces` module is the source).
// Every u64 travels as a canonical decimal string; enums are {type, payload}.

const HEX = Array.from({ length: 256 }, (_, i) => i.toString(16).padStart(2, "0"));

/**
 * An RFC 4122 version 4 UUID from 16 random bytes. `crypto.randomUUID` only
 * exists in secure contexts; `getRandomValues` exists everywhere, so the page
 * also works when it is opened over plain HTTP on a private network.
 */
export function uuidFromBytes(bytes) {
  if (bytes.length !== 16) throw new Error("a UUID needs 16 bytes");
  const b = Array.from(bytes);
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = b.map((x) => HEX[x]).join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

export function randomUuid(cryptoImpl = globalThis.crypto) {
  return uuidFromBytes(cryptoImpl.getRandomValues(new Uint8Array(16)));
}

/** The namespace of every command operation. */
const COMMAND_NAMESPACE = {
  resolve_project: "conversation",
  create_execution: "conversation",
  create_goal: "conversation",
  prepare_existing_thread: "conversation",
  upload_image: "conversation",
  discard_attachment: "conversation",
  send_chat: "conversation",
  continue_chat: "conversation",
  compact_chat: "conversation",
  deliver_follow_ups_next_turn: "conversation",
  withdraw_follow_up: "conversation",
  stop_chat: "execution",
  stop_run: "execution",
  submit_goal: "execution",
  set_goal_review: "execution",
  set_goal_checks_paused: "execution",
  set_agent_web_tools_enabled: "execution",
  answer_user_question: "questions",
  decide_tool_approval: "approvals",
  manage_lane: "lanes",
};

/** The namespace of every query operation. */
const QUERY_NAMESPACE = {
  get_input_receipts: "conversation",
  read_committed_history: "conversation",
  list_projects: "workspace",
  list_threads: "workspace",
  get_thread: "workspace",
  list_runs: "execution",
  list_questions: "questions",
  list_approvals: "approvals",
  get_approval_input: "approvals",
  get_lane_management: "lanes",
  get_core_status: "lifecycle",
  get_command_receipt: "projection",
  read_body: "projection",
  read_snapshot: "projection",
};

function operation(kind, table, type, payload) {
  const namespace = table[type];
  if (!namespace) throw new Error(`unknown ${kind} operation: ${type}`);
  const inner = payload === undefined ? { type } : { type, payload };
  return { kind, payload: { type: namespace, payload: inner } };
}

/** `{kind, payload: {type: namespace, payload: {type, payload}}}` without the ids. */
export function command(type, payload) {
  return operation("command", COMMAND_NAMESPACE, type, payload);
}

export function query(type, payload) {
  return operation("query", QUERY_NAMESPACE, type, payload);
}

/**
 * A UiEvent as `{type, payload}` of the event itself: the namespace level the
 * wire adds is dropped, because an event's name is unique across namespaces.
 * An event that does not have the namespaced shape is returned as an event of
 * unknown type, so the callers' "ignore unknown events" rule applies.
 */
export function flattenEvent(event) {
  const inner = event && typeof event === "object" ? event.payload : null;
  if (!inner || typeof inner !== "object" || typeof inner.type !== "string") {
    return { type: "unknown_event", payload: {} };
  }
  return inner;
}

export function envelope(requestId, sessionId, body) {
  return { request_id: requestId, session_id: sessionId, kind: body.kind, payload: body.payload };
}

export function pageRequest(cursor, limit) {
  const page = { limit };
  if (cursor) page.cursor = cursor;
  return page;
}

/**
 * Turn a reply body into `{ok: true, kind, value}` or `{ok: false, error}`.
 *
 * `value` is the inner value: a CommandReply for a command, the `{type,
 * payload}` QueryReply for a query. Anything that is not a well formed reply
 * is reported as a `transport` error: the answer was lost on the way.
 */
export function parseReply(body) {
  if (!body || typeof body !== "object") return lost("reply is not an object");
  if (body.status === "error") {
    const error = body.payload;
    if (!error || typeof error.code !== "string") return lost("error reply without a code");
    return { ok: false, error };
  }
  if (body.status !== "success" || !body.payload || typeof body.payload.type !== "string") {
    return lost("reply has no status");
  }
  const { type, payload } = body.payload;
  return { ok: true, kind: type, value: payload ?? null };
}

/** An error made on this side: the request or its answer did not arrive. */
export function lost(message) {
  return { ok: false, error: { code: "transport", message, retry: "same_request" } };
}

function mismatch(message) {
  return { ok: false, error: { code: "internal", message, retry: "never" } };
}

/**
 * The value inside a successful query reply of the expected `type`. The reply
 * is `{type: namespace, payload: {type, payload}}`; its inner level names the
 * reply.
 */
export function expectQuery(result, type) {
  if (!result.ok) return result;
  const reply = result.value?.payload;
  if (result.kind !== "query" || !reply || reply.type !== type) {
    return mismatch(`expected a ${type} reply`);
  }
  return { ok: true, value: reply.payload };
}

/** The CommandReply inside a successful command reply. */
export function expectCommand(result) {
  if (!result.ok) return result;
  if (result.kind !== "command" || !result.value) return mismatch("expected a command reply");
  return { ok: true, value: result.value };
}

/**
 * Whether a failed command may have taken effect. Then the client must ask
 * the core (GetCommandReceipt) before it sends again.
 *
 * - `transport`: the request or its answer was lost (network, unreadable
 *   body). The outcome is unknown.
 * - `internal` / `storage_unavailable`: the core failed somewhere after
 *   accepting the request; the outcome is unknown.
 * - Everything else is a decided answer.
 */
export function outcomeUnknown(error) {
  return (
    error.code === "transport" ||
    error.code === "internal" ||
    error.code === "storage_unavailable"
  );
}

// ---- u64 decimals -------------------------------------------------------

const DECIMAL = /^(0|[1-9][0-9]*)$/;

export function isDecimal(text) {
  return typeof text === "string" && text.length <= 20 && DECIMAL.test(text);
}

/** Compare two canonical u64 decimals: -1, 0 or 1. Malformed sorts lowest. */
export function compareDecimal(a, b) {
  const okA = isDecimal(a);
  const okB = isDecimal(b);
  if (!okA || !okB) return okA === okB ? 0 : okA ? 1 : -1;
  if (a.length !== b.length) return a.length < b.length ? -1 : 1;
  return a < b ? -1 : a > b ? 1 : 0;
}

// ---- event cursors ------------------------------------------------------

/** `<epoch>:<sequence>`, as the SSE `id` and the `after` parameter use. */
export function formatCursor(cursor) {
  return `${cursor.core_epoch}:${cursor.sequence}`;
}

export function parseCursor(text) {
  if (typeof text !== "string") return null;
  const parts = text.split(":");
  if (parts.length !== 2 || !isDecimal(parts[0]) || !isDecimal(parts[1])) return null;
  return { core_epoch: parts[0], sequence: parts[1] };
}

/**
 * The protocol's CursorTracker rule: `deliver` a newer event of the same
 * epoch, skip a `duplicate`, and stop on an `epoch_mismatch`.
 */
export function classifyCursor(last, cursor) {
  if (!last || !cursor) return "epoch_mismatch";
  if (cursor.core_epoch !== last.core_epoch) return "epoch_mismatch";
  return compareDecimal(cursor.sequence, last.sequence) <= 0 ? "duplicate" : "deliver";
}

// ---- formatting ---------------------------------------------------------

export function formatBytes(decimal) {
  const n = Number(decimal);
  if (!Number.isFinite(n)) return `${decimal} B`;
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

/** Human text for an error, in Japanese, with the core's message when present. */
export function describeError(error) {
  const label = ERROR_LABELS[error.code] || "エラー";
  return error.message ? `${label}: ${error.message}` : label;
}

const ERROR_LABELS = {
  transport: "通信できませんでした",
  invalid_request: "不正なリクエスト",
  not_found: "見つかりません",
  conflict: "競合しました",
  already_resolved: "既に他のクライアントで解決済みです",
  unsupported_version: "コアのバージョンが対応していません",
  unsupported_capability: "コアがこの機能に対応していません",
  permission_denied: "拒否されました",
  stale_revision: "内容が更新されています",
  stale_epoch: "コアが再起動しています",
  resync_required: "再同期が必要です",
  busy: "混雑しています",
  draining: "コアが停止処理中です",
  limit_exceeded: "上限を超えました",
  storage_unavailable: "ストレージを利用できません",
  restore_blocked: "復元がブロックされています",
  internal: "内部エラー",
};
