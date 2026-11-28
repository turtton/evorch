// Process, environment, wire and database helpers for the PR 20 cutover check.
// Nothing here edits a database, a sidebar or a snapshot; the databases are only
// ever opened read-only, and every wait is for a durable fact, a protocol reply,
// a readiness line or a process exit. The only deadlines are failure watchdogs.
import { spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

/** Failure watchdog (never a success condition). */
export const WATCHDOG_MS = Number(process.env.PR20_CUTOVER_WATCHDOG_MS ?? 300_000);

export function sleepForPoll(ms = 40) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Poll `fn` until it returns a truthy value; the watchdog only reports failure. */
export async function waitFor(description, fn, { watchdogMs = WATCHDOG_MS, alive } = {}) {
  const started = Date.now();
  for (;;) {
    const value = await fn();
    if (value) return value;
    if (alive && !alive()) throw new Error(`process exited while waiting for: ${description}`);
    if (Date.now() - started > watchdogMs) {
      throw new Error(`watchdog (${watchdogMs} ms) expired while waiting for: ${description}`);
    }
    await sleepForPoll();
  }
}

// ---------------------------------------------------------------- environment

/**
 * One directory tree that BOTH the old and the new core run in. Paths are
 * absolute and never change between phases. Sockets live below `run`/`state`,
 * which must stay short (sun_path), so the root is created under a short base.
 */
export class Env {
  constructor(root) {
    this.root = root;
    this.configDir = path.join(root, "config");
    this.cacheDir = path.join(root, "cache");
    this.homeDir = path.join(root, "home");
    this.tmpDir = path.join(root, "tmp");
    this.projectDir = path.join(root, "project");
    this.runDir = path.join(root, "run");
    this.stateDir = path.join(root, "state");
    this.ownersDb = path.join(this.stateDir, "evorch", "threads", "owners.db");
    this.threadsRoot = path.join(this.stateDir, "evorch", "threads");
  }

  static create(base, label) {
    fs.mkdirSync(base, { recursive: true });
    const root = fs.mkdtempSync(path.join(base, `pr20c-${label}-`));
    const env = new Env(root);
    for (const dir of [env.configDir, env.cacheDir, env.homeDir, env.tmpDir, env.projectDir, env.runDir, env.stateDir]) {
      fs.mkdirSync(dir, { recursive: true });
    }
    fs.chmodSync(env.runDir, 0o700);
    fs.chmodSync(env.stateDir, 0o700);
    // A socket below `state`/`run` must fit sun_path (108 incl. NUL).
    const longest = path.join(env.stateDir, "evorch", "threads", `${"x".repeat(32)}.sock`);
    if (Buffer.byteLength(longest) >= 108) {
      throw new Error(`work root too long for ownership sockets (${longest.length} bytes): use PR20_CUTOVER_BASE=/tmp`);
    }
    return env;
  }

  static open(root) {
    return new Env(root);
  }

  /** The provider/routing/project configuration (written once, by the first phase). */
  writeConfig({ baseUrl, keyEnv, extraToml = "" }) {
    fs.writeFileSync(
      path.join(this.configDir, "config.toml"),
      `
[providers.local]
type = "openai-compatible"
base_url = "${baseUrl}"
api_key_env = "${keyEnv}"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.worker]]
profile = "local"
[compaction]
context_window_tokens = 1000000
${extraToml}
`,
    );
    // Offline model catalog (fresh, empty), as the product tests seed it.
    const catalog = JSON.stringify({ fetched_at: Math.floor(Date.now() / 1000), api: {} });
    fs.mkdirSync(path.join(this.cacheDir, "evorch"), { recursive: true });
    fs.writeFileSync(path.join(this.cacheDir, "evorch", "models-dev.json"), catalog);
    fs.mkdirSync(path.join(this.configDir, "catalog"), { recursive: true });
    fs.writeFileSync(path.join(this.configDir, "catalog", "models-dev.json"), catalog);
  }

  /** The environment of a product process: built from nothing, as the product tests do. */
  processEnv(keyEnv, extra = {}) {
    const passthrough = {};
    for (const name of ["PATH", "LD_LIBRARY_PATH"]) {
      if (process.env[name] !== undefined) passthrough[name] = process.env[name];
    }
    return {
      ...passthrough,
      HOME: this.homeDir,
      XDG_RUNTIME_DIR: this.runDir,
      XDG_STATE_HOME: this.stateDir,
      XDG_CACHE_HOME: this.cacheDir,
      XDG_CONFIG_HOME: path.join(this.root, "xdg-config"),
      XDG_DATA_HOME: path.join(this.root, "xdg-data"),
      TMPDIR: this.tmpDir,
      [keyEnv]: "pr20-cutover-offline-key",
      ...extra,
    };
  }
}

export const KEY_ENV = "EVORCH_PR20_PROVIDER_KEY";

// -------------------------------------------------------------------- process

export class Proc {
  /** Spawn and collect output; `lines` handlers see every stdout line. */
  constructor(label, file, args, { cwd, env, logFile }) {
    this.label = label;
    this.file = file;
    this.args = args;
    this.stdout = [];
    this.stderr = [];
    this.readyLines = [];
    this.readyWaiters = [];
    this.exited = null;
    this.logFile = logFile;
    this.child = spawn(file, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"], detached: false });
    this.exitPromise = new Promise((resolve) => {
      this.child.once("exit", (code, signal) => {
        this.exited = { code, signal };
        resolve(this.exited);
        for (const w of this.readyWaiters.splice(0)) w();
      });
    });
    let buffer = "";
    this.child.stdout.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      let i;
      while ((i = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, i);
        buffer = buffer.slice(i + 1);
        this.stdout.push(line);
        this.#log(`[out] ${line}`);
        try {
          const json = JSON.parse(line);
          if (json?.status === "ready") {
            this.readyLines.push(json);
            for (const w of this.readyWaiters.splice(0)) w();
          }
        } catch {
          /* not a JSON line */
        }
      }
    });
    this.child.stderr.on("data", (chunk) => {
      const text = chunk.toString("utf8");
      this.stderr.push(text);
      this.#log(`[err] ${text.trimEnd()}`);
    });
  }

  #log(line) {
    if (this.logFile) fs.appendFileSync(this.logFile, `${new Date().toISOString()} ${this.label} ${line}\n`);
  }

  get pid() {
    return this.child.pid;
  }

  /** The n-th (1-based) readiness line; a planned restart prints another one. */
  async ready(n = 1) {
    while (this.readyLines.length < n) {
      if (this.exited) {
        throw new Error(`${this.label} exited (${JSON.stringify(this.exited)}) before readiness #${n}:\n${this.stderr.join("").slice(-4000)}`);
      }
      await new Promise((resolve) => this.readyWaiters.push(resolve));
    }
    return this.readyLines[n - 1];
  }

  /** Resolves with the exit; the exit event is the only completion signal. */
  wait() {
    return this.exitPromise;
  }

  async kill(signal) {
    if (!this.exited) this.child.kill(signal);
    return this.exitPromise;
  }
}

/** Run a one-shot command (the CLI) to completion. */
export async function runCommand(label, file, args, { cwd, env, logFile, input }) {
  const proc = new Proc(label, file, args, { cwd, env, logFile });
  if (input !== undefined) proc.child.stdin?.end(input);
  const exit = await proc.wait();
  return { ...exit, stdout: proc.stdout.join("\n"), stderr: proc.stderr.join("") };
}

/** True while a PID names a live process (not a zombie we could still reap). */
export function pidAlive(pid) {
  try {
    process.kill(pid, 0);
  } catch {
    return false;
  }
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    return !/^\d+ \(.*\) [ZX] /.test(stat);
  } catch {
    return false;
  }
}

/** Descendants of a pid (for "no process of the old build survives"). */
export function descendants(pid) {
  const out = [];
  const byParent = new Map();
  for (const entry of fs.readdirSync("/proc")) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const stat = fs.readFileSync(`/proc/${entry}/stat`, "utf8");
      const ppid = Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[1]);
      (byParent.get(ppid) ?? byParent.set(ppid, []).get(ppid)).push(Number(entry));
    } catch {
      /* raced exit */
    }
  }
  const stack = [pid];
  while (stack.length) {
    for (const child of byParent.get(stack.pop()) ?? []) {
      out.push(child);
      stack.push(child);
    }
  }
  return out;
}

// ----------------------------------------------------------------------- wire

function frame(value) {
  const payload = Buffer.from(JSON.stringify(value), "utf8");
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length);
  return Buffer.concat([header, payload]);
}

/** A protocol-v2 client over the core's Unix socket (see workbench-protocol). */
export class Wire {
  static async connect(socketPath, name, surface = "desktop") {
    const wire = new Wire(name);
    await wire.#open(socketPath, surface);
    return wire;
  }

  constructor(name) {
    this.name = name;
    this.counter = 0;
    this.pending = new Map();
    this.events = [];
    this.buffer = Buffer.alloc(0);
    this.closed = false;
    this.session = `${name}.s1`;
  }

  async #open(socketPath, surface) {
    this.socket = net.connect(socketPath);
    await new Promise((resolve, reject) => {
      this.socket.once("connect", resolve);
      this.socket.once("error", reject);
    });
    this.socket.on("data", (chunk) => this.#data(chunk));
    this.socket.on("close", () => {
      this.closed = true;
      for (const { reject } of this.pending.values()) reject(new Error("connection closed"));
      this.pending.clear();
    });
    this.socket.on("error", () => {});
    const hello = {
      supported_versions: [2],
      required_capabilities: ["commands.v1", "queries.v1"],
      optional_capabilities: ["snapshot_events.v1", "input_receipts.v1", "approvals.v1", "questions.v1", "committed_bodies.v1", "safe_restart.v1", "conversation.v1"],
      client_build: { name: this.name, version: "1" },
      client_instance_id: this.name,
      local_surface: surface,
    };
    const reply = await new Promise((resolve) => {
      this.handshake = resolve;
      this.socket.write(frame(hello));
    });
    if (reply.type !== "accepted") throw new Error(`handshake rejected: ${JSON.stringify(reply)}`);
    this.server = reply.payload;
  }

  #data(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    while (this.buffer.length >= 4) {
      const len = this.buffer.readUInt32BE(0);
      if (this.buffer.length < 4 + len) return;
      const value = JSON.parse(this.buffer.subarray(4, 4 + len).toString("utf8"));
      this.buffer = this.buffer.subarray(4 + len);
      if (this.handshake) {
        const h = this.handshake;
        this.handshake = null;
        h(value);
      } else if (value.type === "reply" && this.pending.has(value.payload?.request_id)) {
        // Frames are `{type:"reply"|"event"|..., payload}`; a reply wraps
        // `{request_id, status, payload}`.
        const reply = value.payload;
        const { resolve } = this.pending.get(reply.request_id);
        this.pending.delete(reply.request_id);
        resolve(reply);
      } else {
        this.events.push(value);
      }
    }
  }

  request(kind, payload) {
    const id = `${this.name}-${++this.counter}`;
    const promise = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.socket.write(frame({ request_id: id, session_id: this.session, kind, payload }));
    return promise;
  }

  /** command({type:"conversation",payload:{type:"send_chat",payload:{..}}}) */
  command(namespace, op, payload) {
    return this.request("command", { type: namespace, payload: { type: op, payload } });
  }

  query(namespace, op, payload) {
    return this.request("query", { type: namespace, payload: { type: op, payload } });
  }

  close() {
    this.socket.destroy();
  }
}

/**
 * A reply is usable only when it succeeded; the error is surfaced verbatim.
 * Returns the namespaced result (`reply.payload.payload`: a CommandReply for
 * commands, a QueryReply for queries).
 */
export function expectSuccess(label, reply) {
  if (reply.status !== "success") throw new Error(`${label}: ${JSON.stringify(reply).slice(0, 2000)}`);
  return reply.payload?.payload ?? reply.payload;
}

// ------------------------------------------------------------------- database

/** Read-only view of the core database. The path is discovered, never assumed. */
export function findDatabases(env) {
  const found = [];
  const walk = (dir, depth) => {
    if (depth > 6) return;
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p, depth + 1);
      else if (/\.(db|sqlite3?)$/.test(e.name)) found.push(p);
    }
  };
  walk(env.root, 0);
  return found;
}

export class Db {
  constructor(file) {
    this.file = file;
  }

  static async locate(env, { alive } = {}) {
    const file = await waitFor("core database", () => {
      const candidates = findDatabases(env).filter((f) => path.basename(f) !== "owners.db");
      return candidates.find((f) => /evorch|core|storage|state/.test(path.basename(f))) ?? candidates[0];
    }, { alive });
    return new Db(file);
  }

  #open() {
    const db = new DatabaseSync(this.file, { readOnly: true });
    db.exec("PRAGMA busy_timeout = 5000");
    return db;
  }

  all(sql, ...params) {
    const db = this.#open();
    try {
      const statement = db.prepare(sql);
      statement.setReadBigInts(true);
      return statement.all(...params).map((row) => {
        const out = {};
        for (const [k, v] of Object.entries(row)) {
          out[k] = typeof v === "bigint" ? (Number.isSafeInteger(Number(v)) ? Number(v) : v.toString()) : v;
        }
        return out;
      });
    } finally {
      db.close();
    }
  }

  get(sql, ...params) {
    return this.all(sql, ...params)[0];
  }

  /**
   * A reader that stays open, for observing a state that exists only for an
   * instant: `probe(sql)` costs microseconds instead of an open/close.
   */
  persistent() {
    const db = this.#open();
    return {
      get: (sql, ...params) => db.prepare(sql).get(...params),
      close: () => db.close(),
    };
  }

  tables() {
    return this.all("SELECT name FROM sqlite_master WHERE type='table'").map((r) => r.name);
  }

  count(table, where = "1=1", ...params) {
    return this.get(`SELECT COUNT(*) AS n FROM ${table} WHERE ${where}`, ...params).n;
  }
}

export function ownersDbRows(env) {
  if (!fs.existsSync(env.ownersDb)) return null;
  const db = new DatabaseSync(env.ownersDb, { readOnly: true });
  try {
    const tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table'").all().map((r) => r.name);
    const out = { tables, rows: {} };
    for (const t of tables) out.rows[t] = db.prepare(`SELECT * FROM "${t}"`).all();
    return out;
  } finally {
    db.close();
  }
}

export function uid() {
  return os.userInfo().uid;
}
