// Unit tests for the pure parts of the web workbench.
//
// The same file runs in two places:
// - in Chromium, from the browser end-to-end test (tests/web_ui_chromium.rs),
//   which serves it next to the assets under the same relative layout;
// - in Node (CI test job and locally), see run-node.mjs.
// Tests that need a DOM are skipped where there is none.

import { Api } from "../../assets/js/api.js";
import { approvalCard, approvalReady } from "../../assets/js/cards.js";
import { HistoryWindow, messageKey } from "../../assets/js/history.js";
import { parseInline, parseMarkdown, renderMarkdown, safeHref } from "../../assets/js/markdown.js";
import { backoffDelay, EventStream, MAX_BACKOFF_MS } from "../../assets/js/sse.js";
import { composerRoleFor, createStore } from "../../assets/js/store.js";
import { fetchAllPages } from "../../assets/js/paging.js";
import { RereadScheduler } from "../../assets/js/reread.js";
import { coreChanged } from "../../assets/js/format.js";
import { threadView } from "../../assets/js/views.js";
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

const snapshot = (id, items, nextCursor) => ({
  subscription_id: id,
  snapshot_id: `snap-${id}`,
  high_water: { core_epoch: "1", sequence: "5" },
  initial_page: {
    items,
    snapshot_revision: "1",
    ...(nextCursor ? { next_cursor: nextCursor } : {}),
  },
});

test("a snapshot replaces state and a replay keeps it", () => {
  const store = createStore();
  store.applySubscriptionStarted(snapshot("1", [{ type: "thread", payload: thread("t1", "1") }]));
  assert(store.state.threads.has("t1"));
  store.applySubscriptionStarted({ subscription_id: "2", high_water: { core_epoch: "1", sequence: "5" } });
  assert(store.state.threads.has("t1"), "replay keeps state");
  store.applySubscriptionStarted(snapshot("3", [{ type: "thread", payload: thread("t2", "1") }]));
  assert(!store.state.threads.has("t1") && store.state.threads.has("t2"), "snapshot replaces");
  assert(!store.state.partial);
});

test("a cut-short snapshot stays partial until completed", () => {
  const store = createStore();
  store.applySubscriptionStarted(snapshot("1", [{ type: "thread", payload: thread("t1", "1") }], "more"));
  assert(store.state.partial);
  store.completeSnapshot();
  assert(!store.state.partial);
});

test("later snapshot pages fill in without overwriting newer events", () => {
  const store = createStore();
  store.applySubscriptionStarted(snapshot("1", [{ type: "thread", payload: thread("t1", "1") }], "more"));
  const stamp = store.snapshotMark();
  // An event after the snapshot's cut updates t2 before page 2 arrives.
  store.applyEvent({ type: "thread_changed", payload: { thread: thread("t2", "8", "running") } });
  store.applySnapshotPage(
    [
      { type: "thread", payload: thread("t2", "3", "idle") },
      { type: "thread", payload: thread("t3", "4") },
    ],
    stamp,
  );
  eq(store.state.threads.get("t2").state, "running", "the event is newer than the snapshot");
  assert(store.state.threads.has("t3"));
  assert(store.state.partial, "partial until completed");
  store.completeSnapshot();
  assert(!store.state.partial);
  // A page of a snapshot that was replaced since is dropped.
  store.applySubscriptionStarted(snapshot("2", [], "more"));
  eq(store.applySnapshotPage([{ type: "thread", payload: thread("old", "1") }], stamp), false);
  assert(!store.state.threads.has("old"));
});

test("events apply in stream order whatever their revisions", () => {
  const store = createStore();
  const changes = [];
  store.subscribe((c) => changes.push(c.type));
  // Revisions are opaque hashes: a later state may carry a smaller one.
  store.applyEvent({ type: "thread_changed", payload: { thread: thread("t1", "900", "running") } });
  store.applyEvent({ type: "thread_changed", payload: { thread: thread("t1", "17", "idle") } });
  eq(store.state.threads.get("t1").state, "idle", "the later event wins");
  eq(store.applyEvent({ type: "future_event", payload: {} }), null, "unknown events are ignored");
  eq(changes, ["thread", "thread"]);
});

test("a query answer never overwrites a later event", () => {
  const store = createStore();
  const issued = store.mark();
  // While the query is in flight, an event updates the thread.
  store.applyEvent({ type: "thread_changed", payload: { thread: thread("t1", "17", "running") } });
  store.mergePage("threads", [thread("t1", "900", "idle"), thread("t2", "5")], issued);
  eq(store.state.threads.get("t1").state, "running", "the event is newer than the query");
  assert(store.state.threads.has("t2"), "entities the event did not touch are taken");
  // A query sent after the event may replace it.
  store.mergePage("threads", [thread("t1", "3", "failed")], store.mark());
  eq(store.state.threads.get("t1").state, "failed");
});

test("a query sent before a snapshot reset is ignored", () => {
  const store = createStore();
  const issued = store.mark();
  store.applySubscriptionStarted(snapshot("1", [{ type: "thread", payload: thread("t1", "1") }]));
  store.mergePage("threads", [thread("gone", "1")], issued);
  assert(!store.state.threads.has("gone"));
});

test("attention counts follow questions and approvals", () => {
  const store = createStore();
  store.mergePage("threads", [thread("t1", "1"), thread("t2", "1", "idle", "p2")], store.mark());
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
  const before = store.mark();
  store.applyEvent({ type: "question_changed", payload: { question: question("q1", "t1", "0", "yes") } });
  eq(store.totalAttention(), { questions: 0, approvals: 1 }, "an answered question no longer counts");
  store.mergePage("questions", [question("q1", "t1", "99")], before);
  eq(store.totalAttention().questions, 0, "an older page does not bring it back");
});

test("lead-routed questions are the lead's, not the user's", () => {
  const store = createStore();
  const routed = {
    ...question("q2", "lane", "1"),
    target: { type: "lead", payload: { lead_thread: "lead", lead_run: "r" } },
  };
  store.applyEvent({ type: "question_changed", payload: { question: routed } });
  eq(store.totalAttention().questions, 0);
  eq(store.threadAttention("lane").questions, 0);
  eq(store.leadRoutedQuestions("lead").map((q) => q.id), ["q2"]);
  eq(store.leadRoutedQuestions("other"), []);
  // The lead passes it to the user: the same question comes back user-targeted.
  store.applyEvent({ type: "question_changed", payload: { question: { ...routed, target: { type: "user" } } } });
  eq(store.threadAttention("lane").questions, 1);
  eq(store.leadRoutedQuestions(), []);
});

test("the composer role comes from the root run, never from a guess", () => {
  const runs = new Map([
    ["root", { id: "root", role: "orchestrator" }],
    ["w", { id: "w", role: "worker" }],
  ]);
  eq(composerRoleFor(undefined, runs), null, "thread view not read yet");
  eq(composerRoleFor({ root_run: "missing" }, runs), null, "root run not read yet");
  eq(composerRoleFor({ root_run: "root" }, runs), "orchestrator");
  eq(composerRoleFor({ root_run: "w" }, runs), "worker");
  eq(composerRoleFor({}, runs), "choose", "no root run: the person picks");
});

test("approving needs the whole input, unfolded, for exactly this approval revision", () => {
  const visible = { visible: true };
  const view = { id: "ap", core_epoch: "7", revision: "9", state: "pending", summary_truncated: false };
  assert(approvalReady(view, null, visible), "an untruncated summary is the whole input");
  assert(!approvalReady(view, null, { visible: false }), "folded: part of it is hidden");
  assert(!approvalReady(view, null), "not known to be visible");
  const truncated = { ...view, summary_truncated: true };
  const loaded = (over) => ({ key: "ap:7:9", complete: true, blocked: false, ...over });
  assert(!approvalReady(truncated, null, visible), "nothing read yet");
  assert(!approvalReady(truncated, loaded({ complete: false }), visible), "partly read");
  assert(approvalReady(truncated, loaded(), visible), "read to the end and shown");
  assert(!approvalReady(truncated, loaded(), { visible: false }), "read but folded");
  assert(!approvalReady({ ...truncated, revision: "10" }, loaded(), visible), "read for another revision");
  assert(!approvalReady({ ...truncated, core_epoch: "8" }, loaded(), visible), "read in another epoch");
  assert(!approvalReady(truncated, loaded({ blocked: true }), visible), "the core refused the read");
  assert(!approvalReady({ ...view, state: "approved" }, null, visible), "already decided");
});

// ---- paging -------------------------------------------------------------------

test("all pages are read and an expired cursor restarts from the first page", async () => {
  const seen = [];
  let expired = true;
  const load = async (cursor) => {
    seen.push(cursor);
    if (cursor === "c2" && expired) {
      expired = false;
      return { ok: false, error: { code: "conflict" } };
    }
    const next = { null: "c1", c1: "c2", c2: undefined }[String(cursor)];
    return { ok: true, value: { items: [], next_cursor: next } };
  };
  const result = await fetchAllPages(load);
  eq(result, { ok: true, restarts: 1 });
  eq(seen, [null, "c1", "c2", null, "c1", "c2"]);
  const failing = await fetchAllPages(async () => ({ ok: false, error: { code: "busy" } }));
  eq(failing.error.code, "busy");
  const endless = await fetchAllPages(async () => ({ ok: true, value: { items: [], next_cursor: "x" } }), { maxPages: 3 });
  eq(endless.error.code, "limit_exceeded");
});

// ---- history ----------------------------------------------------------------

function message(run, index, text = `${run}:${index}`) {
  return { thread: "t1", run, index: String(index), role: "assistant", content: { type: "inline", payload: { text } } };
}

/** A backward pager like ReadCommittedHistory{direction: "backward"}. */
function pagedHistory(total, size) {
  const all = Array.from({ length: total }, (_, i) => message("r1", i));
  const requests = [];
  const fetchPage = async (cursor) => {
    requests.push(cursor);
    const end = cursor ? Number(cursor.slice(1)) : total;
    const start = Math.max(0, end - size);
    return {
      ok: true,
      value: {
        items: all.slice(start, end),
        next_cursor: start > 0 ? `c${start}` : undefined,
        snapshot_revision: "1",
      },
    };
  };
  return { fetchPage, requests };
}

test("history opens on the newest page and follows cursors to older ones", async () => {
  const { fetchPage, requests } = pagedHistory(10, 3);
  const window = new HistoryWindow(fetchPage);
  assert((await window.open()).ok);
  eq(requests, [null], "one request, not a walk over the whole history");
  eq(window.messages().map((m) => m.index), ["7", "8", "9"]);
  assert(window.hasOlder);
  assert((await window.loadOlder()).ok);
  eq(requests.at(-1), "c7");
  eq(window.messages().map((m) => m.index), ["4", "5", "6", "7", "8", "9"]);
  await window.loadOlder();
  await window.loadOlder();
  eq(window.messages().map((m) => m.index), ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
  assert(!window.hasOlder);
  eq((await window.loadOlder()).added, 0);
});

test("history appends live messages once", async () => {
  const { fetchPage } = pagedHistory(4, 10);
  const window = new HistoryWindow(fetchPage);
  eq(window.append(message("r1", 3)), "held");
  window.append(message("r1", 4));
  await window.open();
  eq(window.append(message("r1", 4)), "ignored", "a duplicate event is dropped");
  eq(window.append(message("r1", 5)), "added");
  eq(window.append(message("r2", 0)), "resync", "a run the page does not show, even at its start");
  eq(window.messages().map(messageKey), ["r1#0", "r1#1", "r1#2", "r1#3", "r1#4", "r1#5"]);
});

test("replayed messages older than the page never land after it", async () => {
  // The page holds messages 180..229 (the newest 50 of 230); a reconnect
  // replays 170..229 and then one new message.
  const { fetchPage } = pagedHistory(230, 50);
  const window = new HistoryWindow(fetchPage);
  await window.open();
  for (let i = 170; i < 230; i += 1) eq(window.append(message("r1", i)), "ignored", `#${i}`);
  eq(window.messages().map((m) => m.index)[0], "180");
  eq(window.messages().length, 50, "nothing older was appended");
  eq(window.append(message("r1", 230)), "added");
  eq(window.messages().at(-1).index, "230");
});

test("each run has its own boundary, and an unplaceable message asks for a re-read", async () => {
  // Two runs interleaved: the page has a0..a2 and b0..b1.
  const items = [message("a", 0), message("b", 0), message("a", 1), message("b", 1), message("a", 2)];
  const window = new HistoryWindow(async () => ({ ok: true, value: { items, snapshot_revision: "1" } }));
  await window.open();
  eq(window.append(message("b", 1)), "ignored");
  eq(window.append(message("a", 1)), "ignored");
  eq(window.append(message("c", 0)), "resync", "an unknown run may be older than the page, even at #0");
  eq(window.append(message("b", 2)), "added", "newer for b even though a is further along");
  eq(window.append(message("a", 3)), "added");
  // Messages above a boundary prove nothing about runs the page lacks.
  eq(window.append(message("c", 0)), "resync", "still no place for a run the page does not show");
  eq(window.append(message("d", 7)), "resync");
  eq(window.messages().map(messageKey), ["a#0", "b#0", "a#1", "b#1", "a#2", "b#2", "a#3"]);
});

test("a late user input of another run is never put after the newest messages", async () => {
  // The page has a#0..a#1 then b#0..b#2. Run a's input was accepted before
  // b#1 but delivered late: the core places it at its acceptance time, so
  // its place on screen is not the end even though a#2 is above a's boundary.
  const items = [message("a", 0), message("a", 1), message("b", 0), message("b", 1), message("b", 2)];
  const window = new HistoryWindow(async () => ({ ok: true, value: { items, snapshot_revision: "1" } }));
  await window.open();
  const late = { ...message("a", 2), role: "user" };
  eq(window.append(late), "resync", "a user input above the boundary has no known place");
  eq(window.append({ ...message("b", 3), role: "user" }), "resync", "nor in the run that is newest");
  eq(window.append(message("b", 3)), "added", "a reply above its run's boundary is new");
  eq(window.append({ ...message("a", 1), role: "user" }), "ignored", "at or below the boundary it is on screen");
  eq(window.messages().map(messageKey), ["a#0", "a#1", "b#0", "b#1", "b#2", "b#3"]);
});

test("a whole older run replayed from #0 is not put after the newest page", async () => {
  // Run a committed a#0, a#1; then run b committed 60 messages. The newest
  // page holds b#10..b#59 only. A reconnect replays from a#0.
  const all = [message("a", 0), message("a", 1), ...Array.from({ length: 60 }, (_, i) => message("b", i))];
  const window = new HistoryWindow(async (cursor) => {
    const end = cursor === null ? all.length : Number(cursor);
    const start = Math.max(0, end - 50);
    return { ok: true, value: { items: all.slice(start, end), next_cursor: start > 0 ? String(start) : undefined, snapshot_revision: "1" } };
  });
  await window.open();
  eq(window.append(message("a", 0)), "resync", "a#0 is older than the page");
  eq(window.append(message("a", 1)), "resync");
  eq(window.append(message("b", 59)), "ignored");
  eq(window.messages().map(messageKey).slice(-2), ["b#58", "b#59"], "nothing was appended after b");
  await window.loadOlder();
  eq(window.messages().map(messageKey).slice(0, 3), ["a#0", "a#1", "b#0"], "the older page puts run a first");
});

test("in an empty history every message needs a read", async () => {
  const window = new HistoryWindow(async () => ({ ok: true, value: { items: [], snapshot_revision: "1" } }));
  await window.open();
  eq(window.append(message("x", 0)), "resync");
});

test("an unplaceable message during the first read makes the read report it", async () => {
  const { fetchPage } = pagedHistory(10, 5);
  const window = new HistoryWindow(fetchPage);
  window.append(message("other", 4));
  eq(await window.open(), { ok: true, resync: true });
});

test("a history error is reported, not thrown", async () => {
  const window = new HistoryWindow(async () => ({ ok: false, error: { code: "not_found" } }));
  eq((await window.open()).error.code, "not_found");
  eq(window.messages(), []);
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

test(
  "a long summary that is the whole input cannot be approved while folded",
  () => {
    const summary = Array.from({ length: 40 }, (_, i) => `line ${i}`).join("\n") + "\nlast line";
    const ctx = { api: {}, store: {}, toast() {}, threadTitle: () => "t", refreshApprovals() {}, maxBodyChunk: () => "65536" };
    const card = approvalCard(ctx, {
      id: "ap", core_epoch: "1", revision: "1", thread: "t", run: "r", state: "pending",
      tool_name: "shell", summary, summary_truncated: false, input_bytes: String(summary.length),
    });
    const pre = card.querySelector(".approval-summary");
    const approve = card.querySelector('button[aria-label="このツール実行を承認"]');
    const toggle = card.querySelector("button.link");
    assert(pre.classList.contains("folded"), "folded at first");
    assert(approve.disabled, "approving is off while folded");
    toggle.click();
    assert(!pre.classList.contains("folded"));
    eq(pre.textContent, summary);
    assert(!approve.disabled, "approving is on once unfolded");
    toggle.click();
    assert(approve.disabled, "folding again turns it off");
  },
  { dom: true },
);

test(
  "loading older history after destroying its thread view does not move the scroll position",
  async () => {
    let finishOlderRead;
    let signalOlderReadStarted;
    const olderReadStarted = new Promise((resolve) => {
      signalOlderReadStarted = resolve;
    });
    let historyReads = 0;
    const store = createStore();
    const ctx = {
      store,
      api: {
        query: async (kind) => {
          if (kind === "get_thread") return new Promise(() => {});
          if (kind !== "read_committed_history") throw new Error(`unexpected query: ${kind}`);
          historyReads += 1;
          if (historyReads === 1) {
            return {
              ok: true,
              value: {
                items: [{
                  thread: "t1",
                  run: "r1",
                  index: "1",
                  role: "assistant",
                  content: { type: "inline", payload: { text: "newest" } },
                }],
                next_cursor: "older",
                snapshot_revision: "1",
              },
            };
          }
          signalOlderReadStarted();
          return new Promise((resolve) => {
            finishOlderRead = resolve;
          });
        },
      },
      queryInto: async () => ({ ok: true, value: { items: [] } }),
      setTitle() {},
      setBack() {},
      maxBodyChunk: () => 65536,
    };

    const view = threadView(ctx, "t1");
    document.body.append(view.el);
    const older = view.el.querySelector("button.older");
    const loaded = () =>
      new Promise((resolve) => {
        if (!older.hidden) {
          resolve();
          return;
        }
        const observer = new MutationObserver(() => {
          if (older.hidden) return;
          observer.disconnect();
          resolve();
        });
        observer.observe(older, { attributes: true, attributeFilter: ["hidden"] });
      });
    let scroller;
    try {
      await loaded();

      scroller = document.scrollingElement;
      let prototype = Object.getPrototypeOf(scroller);
      let nativeScrollTop;
      while (prototype && !nativeScrollTop) {
        nativeScrollTop = Object.getOwnPropertyDescriptor(prototype, "scrollTop");
        prototype = Object.getPrototypeOf(prototype);
      }
      assert(nativeScrollTop?.get && nativeScrollTop?.set, "document scroller exposes scrollTop");
      const scrollWrites = [];
      Object.defineProperty(scroller, "scrollTop", {
        configurable: true,
        get: () => nativeScrollTop.get.call(scroller),
        set: (value) => {
          scrollWrites.push(value);
          nativeScrollTop.set.call(scroller, value);
        },
      });

      older.click();
      await olderReadStarted;
      view.destroy();
      view.el.remove();
      // A replacement screen can now set its own scroll position while the
      // detached thread view's history request is still in flight.
      nativeScrollTop.set.call(scroller, 137);
      scrollWrites.length = 0;
      finishOlderRead({
        ok: true,
        value: {
          items: [{
            thread: "t1",
            run: "r1",
            index: "0",
            role: "assistant",
            content: { type: "inline", payload: { text: "older" } },
          }],
          snapshot_revision: "1",
        },
      });
      // Promise continuations run before the next browser frame; this waits
      // for the resumed click handler without a wall-clock timeout.
      await new Promise(requestAnimationFrame);
      eq(scrollWrites, [], "the destroyed view must not write the document scroll position");
    } finally {
      view.destroy();
      view.el.remove();
      if (scroller) delete scroller.scrollTop;
      finishOlderRead?.({
        ok: true,
        value: { items: [], snapshot_revision: "1" },
      });
    }
  },
  { dom: true },
);

// ---- reread ----------------------------------------------------------------

/** A clock and timers the test advances by hand. */
function manualTime() {
  let now = 0;
  let timers = [];
  return {
    now: () => now,
    setTimer: (fn, ms) => {
      const timer = { at: now + ms, fn };
      timers.push(timer);
      return timer;
    },
    clearTimer: (timer) => {
      timers = timers.filter((t) => t !== timer);
    },
    advance(ms) {
      now += ms;
      for (;;) {
        const due = timers.filter((t) => t.at <= now).sort((a, b) => a.at - b.at)[0];
        if (!due) return;
        timers = timers.filter((t) => t !== due);
        due.fn();
      }
    },
    pending: () => timers.length,
  };
}

function scheduler(time, log) {
  return new RereadScheduler({
    run: () => log.push(`read@${time.now()}`),
    onPaused: (wait) => log.push(`paused ${wait}`),
    debounceMs: 300,
    max: 2,
    windowMs: 10_000,
    now: time.now,
    setTimer: time.setTimer,
    clearTimer: time.clearTimer,
  });
}

test("a burst of unplaced events causes one re-read after the quiet time", () => {
  const time = manualTime();
  const log = [];
  const s = scheduler(time, log);
  s.request();
  time.advance(100);
  s.request();
  s.request();
  time.advance(199);
  eq(log, []);
  time.advance(1);
  eq(log, ["read@300"]);
});

test("over budget the re-read waits and then runs by itself", () => {
  const time = manualTime();
  const log = [];
  const s = scheduler(time, log);
  s.request();
  time.advance(300);
  s.request();
  time.advance(300);
  eq(log, ["read@300", "read@600"]);
  // A third burst within the window: paused until the first read leaves it.
  s.request();
  time.advance(300);
  eq(log.at(-1), "paused 9400", "waits until 300 + 10000");
  assert(s.paused);
  s.request();
  eq(time.pending(), 1, "requests while paused join the waiting read");
  time.advance(9399);
  eq(log.length, 3);
  time.advance(1);
  eq(log.at(-1), "read@10300", "the budget came back: the pending read runs");
  assert(!s.paused);
});

test("a manual reload clears the budget and the waiting read", () => {
  const time = manualTime();
  const log = [];
  const s = scheduler(time, log);
  for (let i = 0; i < 3; i += 1) {
    s.request();
    time.advance(300);
  }
  assert(s.paused);
  s.reset();
  eq(time.pending(), 0);
  s.request();
  time.advance(300);
  eq(log.at(-1), "read@1200");
  s.dispose();
  s.request();
  eq(time.pending(), 0, "nothing after the view is gone");
});

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
  const log = { started: [], events: [], statuses: [], timers: new Map() };
  let ids = 0;
  const stream = new EventStream({
    scope: "all",
    EventSourceImpl: FakeEventSource,
    schedule: (fn, ms, label) => {
      ids += 1;
      log.timers.set(ids, { fn, ms, label });
      return ids;
    },
    cancel: (id) => log.timers.delete(id),
    random: () => 0,
    onStarted: (s) => log.started.push(s),
    onEvent: (e) => log.events.push(e),
    onStatus: (s) => log.statuses.push(s.state),
  });
  const last = () => FakeEventSource.instances.at(-1);
  const pending = (label) => [...log.timers].filter(([, t]) => t.label === label);
  /** Run the oldest pending timer with `label`. */
  const fire = (label = "retry") => {
    const [id, timer] = pending(label)[0];
    log.timers.delete(id);
    timer.fn();
  };
  return { stream, log, last, fire, pending };
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
  const { stream, log, last, fire, pending } = harness();
  stream.start();
  last().emit("subscription_started", { subscription_id: "1", high_water: { core_epoch: "7", sequence: "40" } });
  last().emit("ui_event", ui(41));
  const first = last();
  first.emit("error");
  assert(first.closed);
  eq(log.statuses.at(-1), "waiting");
  eq(pending("retry").map(([, t]) => t.ms), [500]);
  eq(pending("stable").length, 0, "the stability timer is cancelled with the stream");
  fire();
  eq(last().url, "/api/v1/events?scope=all&after=7%3A41");
});

test("repeated failures fall back to a fresh snapshot", () => {
  const { stream, last, fire, pending } = harness();
  stream.start();
  last().emit("subscription_started", { subscription_id: "1", high_water: { core_epoch: "7", sequence: "40" } });
  for (let i = 0; i < 3; i += 1) {
    last().emit("error");
    fire();
  }
  eq(pending("retry").length, 0);
  eq(last().url, "/api/v1/events?scope=all", "no after: the core answers with a snapshot");
});

test("an expired snapshot starts the subscription over without a cursor", () => {
  const { stream, last, fire } = harness();
  stream.start();
  last().emit("subscription_started", { subscription_id: "1", high_water: { core_epoch: "7", sequence: "40" } });
  stream.resubscribe();
  fire();
  eq(last().url, "/api/v1/events?scope=all", "a fresh snapshot, not a replay");
});

test("the backoff starts over only after a stream stayed up", () => {
  const { stream, last, fire, pending } = harness();
  const started = (seq) =>
    last().emit("subscription_started", { subscription_id: "1", high_water: { core_epoch: "7", sequence: String(seq) } });
  const delay = () => pending("retry").at(-1)[1].ms;
  stream.start();
  started(1);
  last().emit("error");
  eq(delay(), 500);
  fire();
  started(1);
  last().emit("error");
  eq(delay(), 1000, "accepted then dropped at once still counts as a failure");
  fire();
  started(1);
  fire("stable");
  last().emit("error");
  eq(delay(), 500, "a stream that stayed up resets the backoff");
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
