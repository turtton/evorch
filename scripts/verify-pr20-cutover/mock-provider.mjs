// Scripted OpenAI-compatible provider (chat completions) for the PR 20 cutover
// check. It is a real HTTP server a real core talks to; nothing here fakes the
// core. Responses are decided by directives inside the LATEST user message:
//
//   [[tool:NAME:BASE64(json)]]   answer with a tool call (generation phase only)
//   [[vtool:NAME:BASE64(json)]]  same, honoured in the verification phase too
//   [[hold:KEY]]                 hold the response until release(KEY) (generation only)
// The tool input is base64 so that a directive can itself be carried inside
// another directive's JSON (a lane brief that scripts the lane root).
//
// Everything else is answered with a short text, which ends the turn. A
// request whose last message is a tool result is answered with text, too.
// The thread a request belongs to is named by a `[[T:name]]` marker in its
// first user message (the driver puts one in every thread it creates and in
// every lane brief); the mock never inspects anything else to pick a script.
import http from "node:http";
import fs from "node:fs";

const DIRECTIVE = /\[\[(tool|vtool):([A-Za-z0-9_]+):([A-Za-z0-9+/=]+)\]\]|\[\[(hold):([A-Za-z0-9_.-]+)\]\]/g;

/** Build a tool directive for a message the mock will read. */
export function toolDirective(name, input, { verify = false } = {}) {
  return `[[${verify ? "vtool" : "tool"}:${name}:${Buffer.from(JSON.stringify(input)).toString("base64")}]]`;
}
export const holdDirective = (key) => `[[hold:${key}]]`;
export const tagMarker = (name) => `[[T:${name}]]`;

function textOf(content) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((part) => (typeof part === "string" ? part : (part?.text ?? "")))
      .join("\n");
  }
  return "";
}

export function tagOf(messages) {
  for (const m of messages) {
    if (m.role !== "user") continue;
    const hit = /\[\[T:([A-Za-z0-9_.-]+)\]\]/.exec(textOf(m.content));
    if (hit) return hit[1];
  }
  return null;
}

/** The thread scratch directory the core names in its system note. */
export function scratchDirOf(messages) {
  for (const m of messages) {
    if (m.role !== "system") continue;
    const hit = /\$TMPDIR is (\S+?)\. Sandboxed/.exec(textOf(m.content));
    if (hit) return hit[1];
  }
  return null;
}

/** Texts of all messages of a request (for counting what the wire carried). */
export function wireTexts(body) {
  return (body.messages ?? []).map((m) => ({ role: m.role, text: textOf(m.content) }));
}

export class MockProvider {
  /** @param {{port?: number, mode: "generate"|"verify", logFile?: string}} options */
  constructor(options) {
    this.mode = options.mode;
    this.port = options.port ?? 0;
    this.logFile = options.logFile;
    this.requests = [];
    this.holds = new Map(); // key -> {arrived, released, waiters}
    this.armed = []; // [{key, predicate}] one-shot holds on a future request
    this.waiters = new Set();
    this.server = http.createServer((req, res) => this.#handle(req, res));
  }

  async start() {
    await new Promise((resolve, reject) => {
      this.server.once("error", reject);
      this.server.listen(this.port, "127.0.0.1", resolve);
    });
    this.port = this.server.address().port;
    return this;
  }

  get baseUrl() {
    return `http://127.0.0.1:${this.port}/v1`;
  }

  async stop() {
    for (const key of this.holds.keys()) this.release(key);
    this.server.closeAllConnections?.();
    await new Promise((resolve) => this.server.close(resolve));
  }

  #hold(key) {
    let hold = this.holds.get(key);
    if (!hold) {
      hold = { arrived: false, released: false, waiters: [] };
      this.holds.set(key, hold);
    }
    return hold;
  }

  /**
   * Hold the NEXT request for which predicate(record) is true (generation only),
   * for requests whose text the driver cannot write itself (a lane notification
   * delivered by the core). Released with release(key) like any hold.
   */
  armHold(key, predicate) {
    this.armed.push({ key, predicate });
  }

  /** Resolves when a request carrying `[[hold:KEY]]` is inside the provider. */
  async arrived(key) {
    const hold = this.#hold(key);
    if (hold.arrived) return;
    await new Promise((resolve) => hold.waiters.push(resolve));
  }

  release(key) {
    const hold = this.#hold(key);
    hold.released = true;
    for (const resolve of hold.waiters.splice(0)) resolve("release");
    this.#notify();
  }

  #notify() {
    for (const waiter of [...this.waiters]) waiter();
  }

  /** Resolves when `predicate(requests)` holds; re-evaluated on every request. */
  /** The most recent request of a thread tag (undefined when none). */
  lastOf(tag) {
    return [...this.requests].reverse().find((r) => r.tag === tag);
  }

  async until(predicate) {
    for (;;) {
      if (predicate(this.requests)) return;
      await new Promise((resolve) => {
        const waiter = () => {
          this.waiters.delete(waiter);
          resolve();
        };
        this.waiters.add(waiter);
      });
    }
  }

  async #handle(req, res) {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const raw = Buffer.concat(chunks).toString("utf8");
    const url = req.url ?? "";
    if (req.method === "GET" && url.endsWith("/models")) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ object: "list", data: [{ id: "mock-model", object: "model" }] }));
      return;
    }
    if (req.method !== "POST" || !url.endsWith("/chat/completions")) {
      res.writeHead(404).end();
      return;
    }
    let body;
    try {
      body = JSON.parse(raw);
    } catch {
      res.writeHead(400).end();
      return;
    }
    const messages = body.messages ?? [];
    const last = messages[messages.length - 1] ?? {};
    const record = {
      n: this.requests.length + 1,
      tag: tagOf(messages),
      lastRole: last.role,
      lastText: textOf(last.content).slice(0, 4000),
      messageCount: messages.length,
      tools: (body.tools ?? []).map((t) => t.function?.name),
      body,
    };
    const plan = this.#plan(record, last);
    if (!plan.hold && this.mode === "generate") {
      const index = this.armed.findIndex((a) => a.predicate(record));
      if (index >= 0) plan.hold = this.armed.splice(index, 1)[0].key;
    }
    record.plan = plan.summary;
    record.hold = plan.hold ?? null;
    this.requests.push(record);
    if (this.logFile) {
      const { body: _omit, ...light } = record;
      fs.appendFileSync(this.logFile, JSON.stringify({ at: Date.now(), ...light }) + "\n");
    }
    this.#notify();
    if (plan.hold) {
      const hold = this.#hold(plan.hold);
      hold.arrived = true;
      for (const resolve of hold.waiters.splice(0)) resolve("arrived");
      // Waiters of `arrived` and of `release` share one list; re-park for release.
      if (!hold.released) {
        await new Promise((resolve) => hold.waiters.push(resolve));
      }
    }
    const aborted = res.destroyed || res.writableEnded;
    if (aborted) return;
    this.#respond(res, body, plan);
  }

  #plan(record, last) {
    if (last.role !== "user") {
      return { text: "mock-ok", summary: `text(after ${last.role})` };
    }
    const text = textOf(last.content);
    const calls = [];
    let hold = null;
    for (const m of text.matchAll(DIRECTIVE)) {
      if (m[1]) {
        if (m[1] === "tool" && this.mode !== "generate") continue;
        try {
          const json = Buffer.from(m[3], "base64").toString("utf8");
          calls.push({ name: m[2], input: JSON.parse(json.replaceAll("{{SCRATCH}}", scratchDirOf(record.body.messages) ?? "{{SCRATCH-NOT-FOUND}}")) });
        } catch (error) {
          return { text: `mock-bad-directive ${error.message}`, summary: "bad-directive" };
        }
      } else if (m[4] === "hold" && this.mode === "generate") {
        hold = m[5];
      }
    }
    if (calls.length) {
      return { calls, hold, summary: `tools(${calls.map((c) => c.name).join(",")})` };
    }
    return { text: "mock-ack", hold, summary: "text(ack)" };
  }

  #respond(res, body, plan) {
    const id = `mock-${Date.now().toString(36)}-${this.requests.length}`;
    const usage = { prompt_tokens: 100, completion_tokens: 5, total_tokens: 105 };
    const toolCalls = (plan.calls ?? []).map((call, index) => ({
      index,
      id: `call_${id}_${index}`,
      type: "function",
      function: { name: call.name, arguments: JSON.stringify(call.input) },
    }));
    const finish = toolCalls.length ? "tool_calls" : "stop";
    if (!body.stream) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          id,
          object: "chat.completion",
          created: 0,
          model: body.model ?? "mock-model",
          choices: [
            {
              index: 0,
              message: {
                role: "assistant",
                content: toolCalls.length ? null : plan.text,
                ...(toolCalls.length ? { tool_calls: toolCalls.map(({ index: _i, ...c }) => c) } : {}),
              },
              finish_reason: finish,
            },
          ],
          usage,
        }),
      );
      return;
    }
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
    const send = (delta, finishReason = null, extra = {}) =>
      res.write(
        `data: ${JSON.stringify({
          id,
          object: "chat.completion.chunk",
          created: 0,
          model: body.model ?? "mock-model",
          choices: [{ index: 0, delta, finish_reason: finishReason }],
          ...extra,
        })}\n\n`,
      );
    send({ role: "assistant", content: toolCalls.length ? null : "" });
    if (toolCalls.length) {
      for (const call of toolCalls) {
        send({
          tool_calls: [
            {
              index: call.index,
              id: call.id,
              type: "function",
              function: { name: call.function.name, arguments: call.function.arguments },
            },
          ],
        });
      }
    } else {
      send({ content: plan.text });
    }
    send({}, finish);
    res.write(`data: ${JSON.stringify({ id, object: "chat.completion.chunk", created: 0, model: body.model ?? "mock-model", choices: [], usage })}\n\n`);
    res.write("data: [DONE]\n\n");
    res.end();
  }
}
