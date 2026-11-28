// Generation phase: the OLD build creates every fixture itself. Nothing is
// written to a database, a sidebar, a snapshot or an owner row by this driver:
// it only talks to a real old core (protocol + the old `evorch` CLI), scripts a
// provider, and then stops the old core. Every state it needs is reached by an
// observed durable fact (a DB row, a readiness line, a provider arrival, a
// process exit), never by elapsed time.
import fs from "node:fs";
import path from "node:path";
import { holdDirective as HD, MockProvider, tagMarker as TM, toolDirective as TD } from "./mock-provider.mjs";
import * as H from "./harness.mjs";
import { driverDir, log, png1x1, readJson, writeJson } from "./common.mjs";

/** A running core plus everything needed to drive it. */
export class Session {
  constructor({ env, artifactDir, mock, label, prefix = "gen" }) {
    this.prefix = prefix; // submission ids must not repeat across phases
    this.env = env;
    this.artifactDir = artifactDir;
    this.mock = mock;
    this.label = label;
    this.submission = 0;
    this.core = null;
    this.wire = null;
    this.db = null;
    this.project = null;
  }

  get coreBin() {
    return path.join(this.artifactDir, "evorch-core");
  }

  get cliBin() {
    return path.join(this.artifactDir, "evorch");
  }

  startCore(label = this.label) {
    this.core = new H.Proc(label, this.coreBin, ["--config-dir", this.env.configDir], {
      cwd: this.env.projectDir,
      env: this.env.processEnv(H.KEY_ENV),
      logFile: path.join(driverDir(this.env.root), "core.log"),
    });
    return this.core;
  }

  async connect() {
    const ready = await this.core.ready(this.core.readyLines.length || 1);
    this.wire?.close();
    this.wire = await H.Wire.connect(ready.socket, `drv${++this.submission}`);
    return ready;
  }

  async openDb() {
    this.db = await H.Db.locate(this.env, { alive: () => !this.core.exited });
    return this.db;
  }

  cli(args, { async = false } = {}) {
    const full = args.concat(["--user-config", this.env.configDir]);
    const options = {
      cwd: this.env.projectDir,
      env: this.env.processEnv(H.KEY_ENV),
      logFile: path.join(driverDir(this.env.root), "cli.log"),
    };
    return async ? new H.Proc("cli", this.cliBin, full, options) : H.runCommand("cli", this.cliBin, full, options);
  }

  /**
   * The project the core itself manages for its working directory. Chat
   * attachments are matched against this one, so it is resolved through the
   * protocol (conversation.resolve_project) instead of registering another.
   */
  async resolveProject() {
    const reply = H.expectSuccess(
      "resolve_project",
      await this.wire.command("conversation", "resolve_project", { root: this.env.projectDir }),
    );
    this.project = reply.result.payload.payload.project;
    return this.project;
  }

  chat(op, thread, text, attachments = []) {
    return this.wire
      .command("conversation", op, {
        project: this.project,
        thread,
        submission_id: `${this.prefix}-${++this.submission}`,
        composer_role: "orchestrator",
        text,
        skills: [],
        attachments,
      })
      .then((reply) => H.expectSuccess(`${op} ${thread}`, reply));
  }

  async uploadImage(thread, filename) {
    const bytes = png1x1();
    const begun = H.expectSuccess(
      "begin_attachment",
      await this.wire.command("conversation", "begin_attachment", {
        project: this.project,
        thread,
        client_upload_id: `${this.prefix}-up-${++this.submission}`,
        filename,
        media_type_hint: "image/png",
        byte_length: bytes.length,
      }),
    );
    const uploadId = begun.result.payload.payload.upload_id;
    H.expectSuccess(
      "append_attachment",
      await this.wire.command("conversation", "append_attachment", {
        upload_id: uploadId,
        offset: 0,
        data_base64: bytes.toString("base64"),
      }),
    );
    const done = H.expectSuccess(
      "finish_attachment",
      await this.wire.command("conversation", "finish_attachment", { upload_id: uploadId }),
    );
    return done.result.payload.payload; // AttachmentRef
  }

  async kill(signal = "SIGKILL") {
    const exit = await this.core.kill(signal);
    return exit;
  }
}

function laneBrief(objective) {
  return {
    objective,
    inputs: [],
    deliverables: ["result"],
    acceptance_criteria: ["checked"],
    constraints: [],
    workspace: {},
    report_policy: "attached",
  };
}

const report = (summary) => TD("lane_report", { status: "ready", summary, artifact_refs: [] });

/** A lead that opens one lane which reports `marker`; `hold` parks the lane first. */
function leadWithLane(threadTag, laneTag, marker, hold) {
  const objective = [TM(laneTag), hold ? HD(hold) : "", report(marker)].filter(Boolean).join(" ");
  return `${TM(threadTag)} open a lane ${TD("lane_open", { brief: laneBrief(objective) })}`;
}

const sql = {
  notifications: "SELECT notification_id, state, claim_epoch, receipt_prompt_id, destination_thread_id, payload FROM lane_notifications",
};

/**
 * Builds fixture "main": a core that has done ordinary work (threads, lanes,
 * attachment, a completed planned restart) and is then killed while three lane
 * destinations are in three different states.
 */
export async function generateMain({ env, artifactDir, results }) {
  const dir = driverDir(env.root);
  const mock = await new MockProvider({ mode: "generate", logFile: path.join(dir, "mock-generate.jsonl") }).start();
  env.writeConfig({ baseUrl: mock.baseUrl, keyEnv: H.KEY_ENV });
  writeJson(path.join(dir, "state.json"), { mockPort: mock.port, fixture: "main", startedAtMs: Date.now() });
  const s = new Session({ env, artifactDir, mock, label: "old-core" });
  const manifest = { fixture: "main", threads: {}, lanes: {}, notifications: {}, attachments: [], scratch: {} };
  try {
    s.startCore();
    await s.connect();
    await s.openDb();
    await s.resolveProject();
    manifest.oldServerHello = { protocol_version: s.wire.server.protocol_version, build: s.wire.server.server_build, capabilities: s.wire.server.capabilities };
    const waitDb = (description, fn) => H.waitFor(description, fn, { alive: () => !s.core.exited });

    // ---- ordinary thread with an acknowledged lane report and an image attachment
    await s.chat("create_execution", "lead-main", leadWithLane("lead-main", "lane-main", "RPT-MAIN lane report"));
    await waitDb("lead-main report acked", () => s.db.count("lane_notifications", "state='acked' AND destination_thread_id='lead-main'") >= 1);
    log("lead-main: lane report delivered and acknowledged");
    const attachment = await s.uploadImage("lead-main", "pr20-cutover.png");
    log(`attachment ref: ${JSON.stringify(attachment)}`);
    manifest.attachments.push({ thread: "lead-main", ...attachment });
    await s.chat("send_chat", "lead-main", "second turn with an image", [attachment.id]);
    await mock.until((rs) =>
      rs.some((r) => r.tag === "lead-main" && JSON.stringify(r.body.messages).includes("image")),
    );
    log("lead-main: attachment reached the provider");

    // ---- a completed planned restart of the OLD build (restart + owner progress rows)
    const restart = s.cli(["core", "restart", "request", "--yes"], { async: true });
    await s.core.ready(2);
    await restart.wait(); // the CLI ends when the core execs; its exit is only the CLI's
    await waitDb("restart completed", () => s.db.count("restart_transactions", "phase='completed'") >= 1);
    await s.connect();
    manifest.restart = s.db.all("SELECT restart_id, phase, revision FROM restart_transactions");
    log(`planned restart completed: ${JSON.stringify(manifest.restart)}`);

    // ---- scratch: a delegated worker writes a file into the thread scratch directory
    const worker = TD("write", { path: "{{SCRATCH}}/pr20-marker.txt", content: "pr20 cutover scratch marker\n" });
    await s.chat(
      "create_execution",
      "lead-scratch",
      `${TM("lead-scratch")} delegate ${TD("delegate", { target: { role: "worker" }, prompt: `${TM("worker-scratch")} ${worker}`, background: false })}`,
    );
    await mock.until((rs) => rs.some((r) => r.tag === "worker-scratch" && r.lastRole === "tool"));
    const scratchDir = (await import("./mock-provider.mjs")).scratchDirOf(
      mock.requests.find((r) => r.tag === "worker-scratch").body.messages,
    );
    manifest.scratch = { dir: scratchDir, marker: scratchDir && path.join(scratchDir, "pr20-marker.txt") };
    log(`lead-scratch: worker wrote the marker in ${scratchDir}`);

    // ---- stopped destination: a lane report is delivered, then the thread is stopped
    await s.chat("create_execution", "lead-stop", leadWithLane("lead-stop", "lane-stop", "RPT-STOP lane report"));
    await waitDb("lead-stop report acked", () => s.db.count("lane_notifications", "state='acked' AND destination_thread_id='lead-stop'") >= 1);
    H.expectSuccess("stop_chat", await s.wire.command("execution", "stop_chat", { thread: "lead-stop" }));
    await waitDb("lead-stop destination stopped", () => s.db.count("lane_delivery_destinations", "thread_id='lead-stop' AND stopped=1") >= 1);
    log("lead-stop: destination stopped");

    // ---- a CLI run that is still inside its provider turn when the core is stopped
    const cliRun = s.cli(
      ["run", "--project", env.projectDir, "--role", "orchestrator", "--prompt", `${TM("cli-run")} ${HD("cli-hold")} cli work`],
      { async: true },
    );
    await mock.arrived("cli-hold");
    manifest.cliRun = { tag: "cli-run", inFlight: true };
    log("cli run: provider turn in flight");

    // ---- receipt NOT created: the lead is inside a provider turn when its lane reports
    await s.chat("create_execution", "lead-rec0", leadWithLane("lead-rec0", "lane-rec0", "RPT-REC0 lane report", "rec0-lane"));
    await mock.arrived("rec0-lane");
    await s.chat("send_chat", "lead-rec0", `${HD("rec0-busy")} keep this turn open`);
    await mock.arrived("rec0-busy");
    mock.release("rec0-lane");
    await waitDb("rec0 notification claimed without receipt", () =>
      s.db.count("lane_notifications", "destination_thread_id='lead-rec0' AND state='pending' AND claim_epoch IS NOT NULL AND receipt_prompt_id IS NULL") >= 1,
    );
    log("lead-rec0: notification claimed, receipt not created, not acknowledged");

    // ---- receipt created but NOT acknowledged. In the old build the receipt commit and the
    // acknowledgement are two storage operations a few microseconds apart, so this state is
    // only observable by watching for it and stopping the core at that very moment. If the
    // acknowledgement wins the race the state is NOT generated and the check stays unverified.
    await s.chat("create_execution", "lead-rec1", leadWithLane("lead-rec1", "lane-rec1", "RPT-REC1 lane report", "rec1-lane"));
    await mock.arrived("rec1-lane");
    await s.chat("send_chat", "lead-rec1", `${HD("rec1-busy")} keep this turn open`);
    await mock.arrived("rec1-busy");
    mock.release("rec1-lane");
    await waitDb("rec1 notification claimed", () =>
      s.db.count("lane_notifications", "destination_thread_id='lead-rec1' AND claim_epoch IS NOT NULL") >= 1,
    );
    // Everything else the fixture needs must already exist before the race starts.
    const rootBinding = path.join(env.threadsRoot, ".evorch-retention-binding.json");
    await waitDb("retention binding on both sides", () => s.db.count("retention_binding") >= 1 && fs.existsSync(rootBinding));
    manifest.notifications = s.db.all(sql.notifications);
    manifest.destinations = s.db.all("SELECT * FROM lane_delivery_destinations");
    manifest.lanes = s.db.all("SELECT lane_id, thread_id FROM lanes");
    manifest.threads = s.db.all("SELECT thread_id FROM threads").map((t) => t.thread_id);
    manifest.project = s.project;
    manifest.corePid = s.core.pid;
    const probe = s.db.persistent();
    const watch =
      "SELECT state, receipt_prompt_id FROM lane_notifications WHERE destination_thread_id='lead-rec1'";
    mock.release("rec1-busy");
    let race = "unobserved";
    for (;;) {
      const row = probe.get(watch);
      if (row?.receipt_prompt_id && row.state === "pending") {
        process.kill(s.core.pid, "SIGKILL"); // stop at the instant the window is open
        race = "caught";
        break;
      }
      if (row?.state === "acked" || row?.state === "obsolete") {
        race = "acknowledged-first";
        break;
      }
      await new Promise((resolve) => setImmediate(resolve));
    }
    probe.close();
    manifest.rec1Race = race;
    log(`lead-rec1 receipt/ack race: ${race}`);

    // ---- stop the old core and prove it is gone
    const exit = await s.kill("SIGKILL");
    manifest.oldCoreExit = exit;
    manifest.cliRun.exit = await cliRun.wait();
    manifest.survivors = H.descendants(manifest.corePid).filter((pid) => H.pidAlive(pid));
    log(`old core stopped: ${JSON.stringify(exit)}; surviving descendants: ${JSON.stringify(manifest.survivors)}`);
    manifest.inventory = inventory(env, s.db, manifest);
    writeJson(path.join(dir, "fixture-main.json"), manifest);
    return manifest;
  } finally {
    if (s.core && !s.core.exited) await s.core.kill("SIGKILL");
    s.wire?.close();
    await mock.stop();
  }
}

/**
 * Inventory of what the old build actually produced. Each entry says how many
 * rows/files exist; an entry with zero is "generated missing" and the report
 * must call the matching check UNVERIFIED.
 */
export function inventory(env, db, manifest) {
  const entry = (name, count, detail) => ({ name, count, generated: count > 0, detail });
  const owners = H.ownersDbRows(env);
  const running = (owners?.rows.thread_owners ?? []).filter((r) => JSON.parse(r.state).state === "Running");
  const ownerOnly = db.all(
    "SELECT run_id FROM run_contexts WHERE restorable=0 AND instr(config_json, 'ownership') > 0",
  );
  const notes = db.all(sql.notifications);
  const bindingRow = db.all("SELECT * FROM retention_binding");
  const rootBinding = path.join(env.threadsRoot, ".evorch-retention-binding.json");
  return [
    entry("owners_running_rows", running.length, running.map((r) => r.thread_id)),
    entry("restart_completed", db.count("restart_transactions", "phase='completed'")),
    entry("restart_owner_progress_rows", db.count("restart_owner_progress")),
    entry("recovery_graph_thread_progress_rows", db.count("recovery_graph_thread_progress")),
    entry("threads", db.count("threads")),
    entry("lanes", db.count("lanes")),
    entry("conversation_runs", db.count("run_contexts")),
    entry("retention_binding_db", bindingRow.length),
    entry("retention_binding_root_file", fs.existsSync(rootBinding) ? 1 : 0, rootBinding),
    entry("attachments_uploaded", manifest.attachments?.length ?? 0, manifest.attachments),
    entry("ownership_only_snapshots", ownerOnly.length, ownerOnly.map((r) => r.run_id)),
    entry(
      "destination_with_owner_generation",
      db.count("lane_delivery_destinations", "instr(owner_json, 'ownership_generation') > 0"),
    ),
    entry(
      "notification_pending_receipt_absent",
      notes.filter((n) => n.state === "pending" && !n.receipt_prompt_id).length,
    ),
    entry(
      "notification_pending_receipt_present",
      notes.filter((n) => n.state === "pending" && n.receipt_prompt_id).length,
    ),
    entry("cli_run_in_flight", manifest.cliRun?.inFlight ? 1 : 0, manifest.cliRun),
    entry("destination_stopped", db.count("lane_delivery_destinations", "stopped=1")),
    entry(
      "scratch_marker_file",
      manifest.scratch?.marker && fs.existsSync(manifest.scratch.marker) ? 1 : 0,
      manifest.scratch,
    ),
  ];
}

/**
 * Builds fixture "graph": a restart of the OLD build that is stopped (SIGKILL)
 * right after its recovery graph was committed and before the successor
 * finished adopting it, next to an unrelated ordinary thread. The row is
 * observed, never edited: the kill is triggered by the restart phase the old
 * core itself wrote. When the restart wins the race and completes first, the
 * fixture is NOT generated (`race: "completed-first"`) and the caller retries
 * with a fresh root.
 */
export async function generateGraph({ env, artifactDir }) {
  const dir = driverDir(env.root);
  const mock = await new MockProvider({ mode: "generate", logFile: path.join(dir, "mock-generate.jsonl") }).start();
  env.writeConfig({ baseUrl: mock.baseUrl, keyEnv: H.KEY_ENV });
  writeJson(path.join(dir, "state.json"), { mockPort: mock.port, fixture: "graph", startedAtMs: Date.now() });
  const s = new Session({ env, artifactDir, mock, label: "old-core" });
  const manifest = { fixture: "graph", attachments: [], scratch: {} };
  try {
    s.startCore();
    await s.connect();
    await s.openDb();
    await s.resolveProject();
    const waitDb = (description, fn) => H.waitFor(description, fn, { alive: () => !s.core.exited });
    await s.chat("create_execution", "lead-graph", leadWithLane("lead-graph", "lane-graph", "RPT-GRAPH lane report"));
    await waitDb("lead-graph report acked", () => s.db.count("lane_notifications", "state='acked' AND destination_thread_id='lead-graph'") >= 1);
    await s.chat("create_execution", "lead-other", `${TM("lead-other")} an unrelated ordinary thread`);
    await mock.until((rs) => rs.some((r) => r.tag === "lead-other"));
    await waitDb("lead-other idle", () => s.db.count("threads", "thread_id='lead-other'") >= 1);
    const rootBinding = path.join(env.threadsRoot, ".evorch-retention-binding.json");
    await waitDb("retention binding on both sides", () => s.db.count("retention_binding") >= 1 && fs.existsSync(rootBinding));
    manifest.project = s.project;
    manifest.corePid = s.core.pid;
    const probe = s.db.persistent();
    const cli = s.cli(["core", "restart", "request", "--yes"], { async: true });
    let phase = null;
    const seen = [];
    for (;;) {
      const row = probe.get("SELECT phase FROM restart_transactions");
      if (row && row.phase !== seen[seen.length - 1]) seen.push(row.phase);
      phase = row?.phase ?? null;
      if (["committed", "armed", "adopted"].includes(phase)) {
        process.kill(s.core.pid, "SIGKILL");
        manifest.race = "caught";
        break;
      }
      if (["completed", "aborted", "invalidated", "superseded"].includes(phase)) {
        manifest.race = "completed-first";
        break;
      }
      if (s.core.exited) throw new Error("old core exited before the restart phase was observed");
      await new Promise((resolve) => setImmediate(resolve));
    }
    probe.close();
    manifest.phasesSeen = seen;
    const exit = await s.kill("SIGKILL");
    await cli.wait();
    manifest.oldCoreExit = exit;
    manifest.survivors = H.descendants(manifest.corePid).filter((pid) => H.pidAlive(pid));
    log(`graph fixture: race=${manifest.race} phases=${seen.join(">")} exit=${JSON.stringify(exit)}`);
    manifest.restart = s.db.all("SELECT restart_id, phase, revision FROM restart_transactions");
    manifest.components = s.db.all("SELECT manifest_id, disposition FROM restart_components");
    manifest.threads = s.db.all("SELECT thread_id FROM threads").map((t) => t.thread_id);
    manifest.inventory = [
      {
        name: "unfinished_restart_graph",
        count: manifest.race === "caught" ? manifest.restart.filter((r) => r.phase !== "completed").length : 0,
        generated: manifest.race === "caught",
        detail: manifest.restart,
      },
      {
        name: "retry_pending_components",
        count: manifest.components.filter((c) => c.disposition === "retry_pending").length,
        generated: manifest.components.some((c) => c.disposition === "retry_pending"),
      },
      { name: "unrelated_ordinary_thread", count: manifest.threads.includes("lead-other") ? 1 : 0, generated: manifest.threads.includes("lead-other") },
      { name: "owners_running_rows", count: (H.ownersDbRows(env)?.rows.thread_owners ?? []).length, generated: !!H.ownersDbRows(env)?.rows.thread_owners?.length },
    ];
    writeJson(path.join(dir, "fixture-graph.json"), manifest);
    return manifest;
  } finally {
    if (s.core && !s.core.exited) await s.core.kill("SIGKILL");
    s.wire?.close();
    await mock.stop();
  }
}
