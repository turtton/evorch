// Unit tests for the pure parts of the web workbench.
//
// The same file runs in two places:
// - in Chromium, from the browser end-to-end test (tests/web_ui_chromium.rs),
//   which serves it next to the assets under the same relative layout;
// - in Node (CI test job and locally), see run-node.mjs.
// Tests that need a DOM are skipped where there is none.

import { Api } from "../../assets/js/api.js";
import { HistoryWindow, messageKey } from "../../assets/js/history.js";
import { parseInline, parseMarkdown, renderMarkdown, safeHref } from "../../assets/js/markdown.js";
import { backoffDelay, EventStream, MAX_BACKOFF_MS } from "../../assets/js/sse.js";
import { createStore } from "../../assets/js/store.js";
import { coreChanged } from "../../assets/js/format.js";
import {
  classifyCursor,
  command,
  compareDecimal,
  envelope,
  expectQuery,
  formatCursor,
  outcomeUnknown,
  parseCursor,
  parseReply,
  uuidFromBytes,
} from "../../assets/js/wire.js";

const tests = [];
function test(name, fn, { dom = false } = {}) {
  tests.push({ name, fn, dom });
}

class AssertionError extends Error {}

function assert(condition, message = "assertion failed") {
  if (!condition) throw new AssertionError(message);
}

function eq(actual, expected, message = "") {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new AssertionError(`${message} expected ${b}, got ${a}`);
}

// ---- wire -------------------------------------------------------------------

test("uuid has version 4 and variant bits", () => {
  const id = uuidFromBytes(new Uint8Array(16).fill(0xff));
  eq(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
  assert(/^[0-9a-f-]{36}$/.test(uuidFromBytes(new Uint8Array(16))));
});

test("request envelope matches the golden shape", () => {
  const body = command("stop_chat", { thread: "thr-1" });
  eq(envelope("req-1", "sess-1", body), {
    request_id: "req-1",
    session_id: "sess-1",
    kind: "command",
    payload: { type: "stop_chat", payload: { thread: "thr-1" } },
  });
  eq(command("get_core_status").payload, { type: "get_core_status" });
});

test("replies parse into success, error or transport failure", () => {
  const ok = parseReply({
    request_id: "req-1",
    status: "success",
    payload: { type: "query", payload: { type: "threads", payload: { items: [], snapshot_revision: "1" } } },
  });
  assert(ok.ok);
  eq(expectQuery(ok, "threads").value.items, []);
  assert(!expectQuery(ok, "projects").ok, "wrong reply type is refused");
  const error = parseReply({ status: "error", payload: { code: "stale_revision", message: "m", retry: "after_resync" } });
  eq(error.error.code, "stale_revision");
  eq(parseReply(null).error.code, "transport");
  eq(parseReply({ status: "weird" }).error.code, "transport");
});

test("only undecided failures need a receipt check", () => {
  assert(outcomeUnknown({ code: "transport" }));
  assert(outcomeUnknown({ code: "internal" }));
  assert(outcomeUnknown({ code: "storage_unavailable" }));
  assert(!outcomeUnknown({ code: "stale_revision" }));
  assert(!outcomeUnknown({ code: "busy" }));
});

test("u64 decimals compare beyond 2^53", () => {
  eq(compareDecimal("9007199254740993", "9007199254740992"), 1);
  eq(compareDecimal("18446744073709551615", "18446744073709551615"), 0);
  eq(compareDecimal("2", "10"), -1);
  eq(compareDecimal("01", "1"), -1, "a malformed value sorts lowest");
});

test("cursors round trip and follow the tracker rule", () => {
  const cursor = { core_epoch: "7", sequence: "18446744073709551615" };
  eq(parseCursor(formatCursor(cursor)), cursor);
  for (const bad of ["", "7", "7:", ":1", "07:1", "a:b", "7:1:2"]) eq(parseCursor(bad), null, bad);
  const last = { core_epoch: "7", sequence: "40" };
  eq(classifyCursor(last, { core_epoch: "7", sequence: "41" }), "deliver");
  eq(classifyCursor(last, { core_epoch: "7", sequence: "40" }), "duplicate");
  eq(classifyCursor(last, { core_epoch: "7", sequence: "9" }), "duplicate");
  eq(classifyCursor(last, { core_epoch: "8", sequence: "1" }), "epoch_mismatch");
});

test("a new build, core or epoch counts as a restart", () => {
  const hello = { core: "c", instance_epoch: "7", server_build: { name: "e", version: "1", revision: "a" } };
  assert(!coreChanged(hello, structuredClone(hello)));
  assert(coreChanged(hello, { ...hello, instance_epoch: "8" }));
  assert(coreChanged(hello, { ...hello, server_build: { ...hello.server_build, revision: "b" } }));
});

// ---- store ------------------------------------------------------------------

const thread = (id, revision, state = "idle", project = "p1") => ({
  id,
  project,
  title: id,
  state,
  revision,
  updated_at: "1",
});

const question = (id, threadId, revision, answer) => ({
  id,
  requester: { run: "r", thread: threadId, root_run: "r" },
  target: { type: "user" },
  management_revision: "1",
  routing_revision: "1",
  blocking: true,
  title: "?",
  options: [],
  revision,
  raised_by: { actor: "agent", surface: "none", agent_run_id: "r" },
  ...(answer ? { answer: { text: answer } } : {}),
});

test("a snapshot replaces state and a replay keeps it", () => {
  const store = createStore();
  store.applySubscriptionStarted({
    subscription_id: "1",
    snapshot_id: "s1",
    high_water: { core_epoch: "1", sequence: "5" },
    initial_page: { items: [{ type: "thread", payload: thread("t1", "1") }], snapshot_revision: "1" },
  });
  assert(store.state.threads.has("t1"));
  store.applySubscriptionStarted({ subscription_id: "2", high_water: { core_epoch: "1", sequence: "5" } });
  assert(store.state.threads.has("t1"), "replay keeps state");
  store.applySubscriptionStarted({
    subscription_id: "3",
    snapshot_id: "s2",
    high_water: { core_epoch: "2", sequence: "0" },
    initial_page: { items: [{ type: "thread", payload: thread("t2", "1") }], snapshot_revision: "1" },
  });
  assert(!store.state.threads.has("t1") && store.state.threads.has("t2"), "snapshot replaces");
});

test("events apply by revision and are idempotent", () => {
  const store = createStore();
  const changes = [];
  store.subscribe((c) => changes.push(c.type));
  assert(store.applyEvent({ type: "thread_changed", payload: { thread: thread("t1", "5", "running") } }));
  assert(store.applyEvent({ type: "thread_changed", payload: { thread: thread("t1", "5", "running") } }), "same revision applies again");
  eq(store.applyEvent({ type: "thread_changed", payload: { thread: thread("t1", "4", "idle") } }), null, "older is ignored");
  eq(store.state.threads.get("t1").state, "running");
  eq(store.applyEvent({ type: "future_event", payload: {} }), null, "unknown events are ignored");
  eq(changes, ["thread", "thread"]);
});

test("attention counts follow questions and approvals", () => {
  const store = createStore();
  store.mergePage("threads", [thread("t1", "1"), thread("t2", "1", "idle", "p2")]);
  store.applyEvent({ type: "question_changed", payload: { question: question("q1", "t1", "1") } });
  store.applyEvent({
    type: "approval_changed",
    payload: {
      approval: { id: "a1", core_epoch: "1", revision: "1", thread: "t2", run: "r", tool_name: "shell", summary: "ls", state: "pending" },
    },
  });
  eq(store.threadAttention("t1"), { questions: 1, approvals: 0 });
  eq(store.projectAttention("p2"), { questions: 0, approvals: 1 });
  eq(store.totalAttention(), { questions: 1, approvals: 1 });
  store.applyEvent({ type: "question_changed", payload: { question: question("q1", "t1", "2", "yes") } });
  eq(store.totalAttention(), { questions: 0, approvals: 1 }, "an answered question no longer counts");
  store.mergePage("questions", [question("q1", "t1", "1")]);
  eq(store.totalAttention().questions, 0, "a stale page does not bring it back");
});

// ---- history ----------------------------------------------------------------

function message(run, index, text = `${run}:${index}`) {
  return { thread: "t1", run, index: String(index), role: "assistant", content: { type: "inline", payload: { text } } };
}

function pagedHistory(total, size) {
  const all = Array.from({ length: total }, (_, i) => message("r1", i));
  const requests = [];
  const fetchPage = async (cursor) => {
    requests.push(cursor);
    const start = cursor ? Number(cursor.slice(1)) : 0;
    const end = Math.min(total, start + size);
    return {
      ok: true,
      value: {
        items: all.slice(start, end),
        next_cursor: end < total ? `c${end}` : undefined,
        snapshot_revision: "1",
      },
    };
  };
  return { fetchPage, requests };
}

test("history shows the newest pages and loads older ones with their cursors", async () => {
  const { fetchPage, requests } = pagedHistory(10, 3);
  const window = new HistoryWindow(fetchPage);
  assert((await window.open()).ok);
  eq(requests, [null, "c3", "c6", "c9"]);
  eq(window.messages().map((m) => m.index), ["6", "7", "8", "9"]);
  assert(window.hasOlder);
  assert((await window.loadOlder()).ok);
  eq(requests.at(-1), "c3", "the page before is read again with its own cursor");
  eq(window.messages().map((m) => m.index), ["3", "4", "5", "6", "7", "8", "9"]);
  await window.loadOlder();
  eq(window.messages()[0].index, "0");
  assert(!window.hasOlder);
});

test("history appends live messages once", async () => {
  const { fetchPage } = pagedHistory(4, 10);
  const window = new HistoryWindow(fetchPage);
  window.append(message("r1", 3));
  window.append(message("r1", 4));
  await window.open();
  assert(!window.append(message("r1", 4)), "a duplicate event is dropped");
  assert(window.append(message("r2", 0)));
  eq(window.messages().map(messageKey), ["r1#0", "r1#1", "r1#2", "r1#3", "r1#4", "r2#0"]);
});

test("a history error is reported, not thrown", async () => {
  const window = new HistoryWindow(async () => ({ ok: false, error: { code: "not_found" } }));
  eq((await window.open()).error.code, "not_found");
});

// ---- markdown -----------------------------------------------------------------

function flatText(nodes) {
  return nodes.map((n) => (n.t === "text" ? n.v : n.c ? flatText(n.c) : n.t === "code" ? n.v : "")).join("");
}

test("only absolute http and https links survive", () => {
  eq(safeHref("https://example.com/a?b#c"), "https://example.com/a?b#c");
  eq(safeHref("http://example.com"), "http://example.com/");
  for (const bad of ["javascript:alert(1)", "JAVASCRIPT:alert(1)", "data:text/html,x", "//evil.example", "/local", "vbscript:x", " java\nscript:x", "file:///etc/passwd"]) {
    eq(safeHref(bad), null, bad);
  }
  const nodes = parseInline("[x](javascript:alert(1)) [y](https://ok.example)", 0);
  assert(!nodes.some((n) => n.t === "link" && !n.href.startsWith("https://")), "no unsafe link");
  assert(nodes.some((n) => n.t === "link" && n.href === "https://ok.example/"));
});

test("raw HTML stays literal text", () => {
  const blocks = parseMarkdown('<img src=x onerror="alert(1)"><script>alert(2)</script>');
  eq(blocks.length, 1);
  eq(blocks[0].t, "p");
  eq(flatText(blocks[0].c), '<img src=x onerror="alert(1)"><script>alert(2)</script>');
});

test("images become links that are never loaded", () => {
  const nodes = parseInline("![logo](https://example.com/a.png)", 0);
  eq(nodes, [{ t: "link", href: "https://example.com/a.png", image: true, c: [{ t: "text", v: "画像: logo" }] }]);
});

test("block structure parses", () => {
  const blocks = parseMarkdown("# Title\n\npara **bold** and *em* `code`\nnext\n\n- a\n- b\n\n1. x\n2. y\n\n> quote\n\n```rust\nfn main() {}\n```\n\n---");
  eq(blocks.map((b) => b.t), ["h", "p", "ul", "ol", "quote", "code", "hr"]);
  eq(blocks[0].level, 1);
  eq(blocks[1].c.map((n) => n.t), ["text", "strong", "text", "em", "text", "code", "br", "text"]);
  eq(blocks[2].items.length, 2);
  eq(blocks[5], { t: "code", lang: "rust", v: "fn main() {}" });
});

test("bare URLs drop trailing punctuation", () => {
  const nodes = parseInline("see https://example.com/x). and (https://example.com/y)", 0);
  const links = nodes.filter((n) => n.t === "link").map((n) => n.href);
  eq(links, ["https://example.com/x", "https://example.com/y"]);
});

test("bare URLs end where Japanese text starts", () => {
  const nodes = parseInline("詳細はhttps://example.com/docsを参照", 0);
  eq(nodes.map((n) => (n.t === "link" ? `[${n.href}]` : n.v)).join(""), "詳細は[https://example.com/docs]を参照");
  assert(!parseInline("xhttps://example.com", 0).some((n) => n.t === "link"), "not inside an ASCII word");
});

test("unmatched delimiters stay text and parsing finishes", () => {
  const text = "a *b _c ~~d ".repeat(10000) + "[".repeat(2000);
  const blocks = parseMarkdown(text);
  eq(flatText(blocks[0].c), text);
  eq(flatText(parseInline("a * b ** c ~~", 0)), "a * b ** c ~~");
});

test(
  "rendered markdown has no scriptable elements or attributes",
  () => {
    const fragment = renderMarkdown(
      '<img src=x onerror=alert(1)> [a](https://ok.example "t") [b](javascript:alert(1)) ![i](https://img.example/x.png)\n\n```\n<b>x</b>\n```',
    );
    const host = document.createElement("div");
    host.append(fragment);
    const allowed = new Set(["P", "A", "PRE", "CODE", "BR", "STRONG", "EM", "DEL", "UL", "OL", "LI", "BLOCKQUOTE", "HR", "H3", "H4", "H5", "H6", "SPAN"]);
    for (const el of host.querySelectorAll("*")) {
      assert(allowed.has(el.tagName), `unexpected <${el.tagName}>`);
      for (const attr of el.attributes) {
        assert(["href", "rel", "target", "referrerpolicy", "class", "start"].includes(attr.name), `unexpected ${attr.name}`);
      }
    }
    assert(host.querySelector("img") === null, "no image is loaded");
    const links = [...host.querySelectorAll("a")];
    eq(links.map((a) => a.href), ["https://ok.example/", "https://img.example/x.png"]);
    for (const a of links) {
      eq(a.rel, "noopener noreferrer");
      eq(a.target, "_blank");
    }
    assert(host.textContent.includes("<img src=x onerror=alert(1)>"));
    eq(host.querySelector("pre code").textContent, "<b>x</b>");
  },
  { dom: true },
);

// ---- sse ----------------------------------------------------------------------

test("backoff grows, is jittered and capped", () => {
  eq(backoffDelay(0, () => 0), 500);
  eq(backoffDelay(0, () => 1), 1000);
  eq(backoffDelay(3, () => 1), 8000);
  eq(backoffDelay(30, () => 1), MAX_BACKOFF_MS);
  assert(backoffDelay(30, () => 0) >= MAX_BACKOFF_MS / 2);
});

class FakeEventSource {
  static instances = [];
  constructor(url) {
    this.url = url;
    this.listeners = {};
    this.closed = false;
    FakeEventSource.instances.push(this);
  }
  addEventListener(name, fn) {
    (this.listeners[name] ??= []).push(fn);
  }
  close() {
    this.closed = true;
  }
  emit(name, data) {
    for (const fn of this.listeners[name] ?? []) fn({ data: data === undefined ? undefined : JSON.stringify(data) });
  }
}

function harness() {
  FakeEventSource.instances = [];
  const log = { started: [], events: [], statuses: [], timers: [] };
  const stream = new EventStream({
    scope: "all",
    EventSourceImpl: FakeEventSource,
    schedule: (fn, ms) => {
      log.timers.push({ fn, ms });
      return log.timers.length;
    },
    cancel: () => {},
    random: () => 0,
    onStarted: (s) => log.started.push(s),
    onEvent: (e) => log.events.push(e),
    onStatus: (s) => log.statuses.push(s.state),
  });
  const last = () => FakeEventSource.instances.at(-1);
  const fire = () => log.timers.shift().fn();
  return { stream, log, last, fire };
}

const ui = (sequence, type = "thread_changed", epoch = "7", payload = {}) => ({
  subscription_id: "1",
  cursor: { core_epoch: epoch, sequence: String(sequence) },
  event: { type, payload },
});

test("the stream applies new events and skips duplicates", () => {
  const { stream, log, last } = harness();
  stream.start();
  eq(last().url, "/api/v1/events?scope=all");
  last().emit("subscription_started", { subscription_id: "1", snapshot_id: "s", high_water: { core_epoch: "7", sequence: "40" } });
  last().emit("ui_event", ui(40));
  last().emit("ui_event", ui(41));
  last().emit("ui_event", ui(41));
  eq(log.events.map((e) => e.cursor.sequence), ["41"]);
  eq(log.statuses, ["connecting", "live"]);
});

test("a disconnect waits, then resumes after the last cursor", () => {
  const { stream, log, last, fire } = harness();
  stream.start();
  last().emit("subscription_started", { subscription_id: "1", high_water: { core_epoch: "7", sequence: "40" } });
  last().emit("ui_event", ui(41));
  const first = last();
  first.emit("error");
  assert(first.closed);
  eq(log.statuses.at(-1), "waiting");
  eq(log.timers[0].ms, 500);
  fire();
  eq(last().url, "/api/v1/events?scope=all&after=7%3A41");
});

test("repeated failures fall back to a fresh snapshot", () => {
  const { stream, log, last, fire } = harness();
  stream.start();
  last().emit("subscription_started", { subscription_id: "1", high_water: { core_epoch: "7", sequence: "40" } });
  for (let i = 0; i < 3; i += 1) {
    last().emit("error");
    fire();
  }
  eq(log.timers.length, 0);
  eq(last().url, "/api/v1/events?scope=all", "no after: the core answers with a snapshot");
});

test("resync and epoch change reconnect for a snapshot", () => {
  const { stream, last, fire } = harness();
  stream.start();
  last().emit("subscription_started", { subscription_id: "1", high_water: { core_epoch: "7", sequence: "40" } });
  last().emit("ui_event", ui(40, "resync_required", "7", { reason: "slow_subscriber" }));
  fire();
  eq(last().url, "/api/v1/events?scope=all&after=7%3A40", "a slow subscriber may replay");
  last().emit("subscription_started", { subscription_id: "2", high_water: { core_epoch: "7", sequence: "40" } });
  last().emit("ui_event", ui(40, "resync_required", "7", { reason: "cursor_too_old" }));
  fire();
  eq(last().url, "/api/v1/events?scope=all");
  last().emit("subscription_started", { subscription_id: "3", high_water: { core_epoch: "7", sequence: "50" } });
  last().emit("ui_event", ui(1, "thread_changed", "8"));
  fire();
  eq(last().url, "/api/v1/events?scope=all", "another epoch needs a snapshot");
});

// ---- api ----------------------------------------------------------------------

function fakeFetch(script) {
  const seen = [];
  const impl = async (url, init) => {
    const body = init?.body ? JSON.parse(init.body) : null;
    seen.push(body);
    const next = script.shift();
    if (next === "network") throw new TypeError("network");
    return { status: 200, json: async () => next(body) };
  };
  return { impl, seen };
}

const commandOk = (body) => ({
  request_id: body.request_id,
  status: "success",
  payload: { type: "command", payload: { disposition: "accepted" } },
});
const receipt = (state) => (body) => ({
  request_id: body.request_id,
  status: "success",
  payload: {
    type: "query",
    payload: { type: "command_receipt", payload: { request_id: body.payload.payload.request_id, state } },
  },
});

test("a lost command is checked before it is sent again with the same ids", async () => {
  const { impl, seen } = fakeFetch(["network", receipt({ type: "unknown" }), commandOk]);
  const api = new Api({ fetchImpl: impl, sessionId: "s-1" });
  const body = command("send_chat", { project: "p", thread: "t", submission_id: "sub-1", text: "hi" });
  const result = await api.sendCommand(body, { requestId: "req-1" });
  assert(result.ok);
  eq(seen.map((r) => r.kind), ["command", "query", "command"]);
  eq(seen[1].payload, { type: "get_command_receipt", payload: { request_id: "req-1" } });
  eq(seen[2].request_id, "req-1");
  eq(seen[2].payload.payload.submission_id, "sub-1");
  eq(seen[0].session_id, "s-1");
});

test("an applied receipt is not sent again", async () => {
  const applied = receipt({ type: "applied", payload: { reply: { disposition: "already_applied" } } });
  const { impl, seen } = fakeFetch(["network", applied]);
  const api = new Api({ fetchImpl: impl, sessionId: "s-1" });
  const result = await api.sendCommand(command("stop_chat", { thread: "t" }));
  eq(result.value, { disposition: "already_applied" });
  eq(seen.length, 2);
});

test("a decided refusal is final and an unreachable core leaves the outcome unknown", async () => {
  const refusal = (body) => ({ request_id: body.request_id, status: "error", payload: { code: "stale_revision", message: "m", retry: "after_resync" } });
  let api = new Api({ fetchImpl: fakeFetch([refusal]).impl, sessionId: "s" });
  let result = await api.sendCommand(command("stop_chat", { thread: "t" }));
  eq([result.ok, result.unknown, result.error.code], [false, undefined, "stale_revision"]);
  api = new Api({ fetchImpl: fakeFetch(["network", "network"]).impl, sessionId: "s" });
  result = await api.sendCommand(command("stop_chat", { thread: "t" }), { requestId: "req-9" });
  eq([result.ok, result.unknown, result.requestId], [false, true, "req-9"]);
});

// ---- runner -------------------------------------------------------------------

/** Run every test. Resolves `{passed, skipped, failures: [{name, message}]}`. */
export async function run({ hasDom = typeof document !== "undefined" } = {}) {
  let passed = 0;
  let skipped = 0;
  const failures = [];
  for (const t of tests) {
    if (t.dom && !hasDom) {
      skipped += 1;
      continue;
    }
    try {
      await t.fn();
      passed += 1;
    } catch (error) {
      failures.push({ name: t.name, message: String(error?.stack ?? error) });
    }
  }
  return { passed, skipped, failures };
}
