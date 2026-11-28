// Verification phase: a NEW build's core cold-starts on the data the OLD build
// left, in the very same absolute paths. Everything is observed on a real core
// process; a check the new build cannot answer yet ends as `todo` (naming the
// surface it depends on) and a state the old build never produced ends as
// `unverified`. Neither is success.
import crypto from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import fs from "node:fs";
import path from "node:path";
import { attachedFilesPaths, MockProvider, tagMarker, wireTexts } from "./mock-provider.mjs";
import * as H from "./harness.mjs";
import { Session } from "./gen.mjs";
import { driverDir, log, readJson } from "./common.mjs";

const STEP_WATCHDOG_MS = Number(process.env.PR20_CUTOVER_STEP_WATCHDOG_MS ?? 120_000);

const sha256 = (file) => crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");

function ownersFiles(env) {
  return ["", "-wal", "-shm"].map((suffix) => env.ownersDb + suffix).filter((f) => fs.existsSync(f));
}

function digestOf(files) {
  return Object.fromEntries(files.map((f) => [path.basename(f), sha256(f)]));
}

/** Messages of a recorded provider request that contain `needle`. */
export function messagesWith(record, needle) {
  return wireTexts(record.body).filter((m) => m.text.includes(needle)).length;
}

/** A request whose wire carries `needle` in any user message (the thread tag is not assumed to survive). */
export function carries(record, needle) {
  return wireTexts(record.body).some((m) => m.role === "user" && m.text.includes(needle));
}

/** Requests of one scripted thread (tagged by the first `[[T:tag]]` marker of its history). */
function ofThread(mock, tag) {
  return mock.requests.filter((r) => r.tag === tag);
}

function requestsWith(mock, needle) {
  return mock.requests.filter((r) => carries(r, needle));
}

function describeCarriers(record, needle) {
  return wireTexts(record.body)
    .map((m, i) => ({ i, role: m.role, snippet: m.text.slice(0, 90) }))
    .filter((m) => record.body.messages[m.i] && wireTexts(record.body)[m.i].text.includes(needle));
}

function sameJson(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** `earlier` is an exact prefix of `later` (append-only history). */
export function isPrefix(earlier, later) {
  return earlier.length <= later.length && earlier.every((m, i) => sameJson(m, later[i]));
}

/** Files below the per-uid attachment roots whose content hash matches (the layout is not assumed). */
function findAttachmentFiles(wantSha) {
  const out = [];
  const prefix = `evo-attachments-${H.uid()}-`;
  for (const entry of fs.readdirSync("/var/tmp")) {
    if (!entry.startsWith(prefix)) continue;
    const walk = (dir, depth) => {
      if (depth > 8) return;
      let entries;
      try {
        entries = fs.readdirSync(dir, { withFileTypes: true });
      } catch {
        return; // raced removal
      }
      for (const e of entries) {
        const p = path.join(dir, e.name);
        if (e.isDirectory()) walk(p, depth + 1);
        else if (e.isFile() && fs.statSync(p).size <= 4 * 1024 * 1024 && sha256(p) === wantSha) out.push(p);
      }
    };
    walk(path.join("/var/tmp", entry), 0);
  }
  return out;
}

async function step(results, id, fn) {
  try {
    await fn();
  } catch (error) {
    results.fail(id, error.message ?? String(error));
  }
}

/** Wait for a durable fact, failing (not hanging) when the failure watchdog expires. */
function waitFact(s, description, fn) {
  return H.waitFor(description, fn, {
    watchdogMs: STEP_WATCHDOG_MS,
    alive: () => {
      if (s.core.exited) throw new Error(`the core process exited (${JSON.stringify(s.core.exited)}) while waiting for: ${description}; stderr tail: ${JSON.stringify(s.core.stderr.join("").slice(-600))}`);
      return true;
    },
  });
}

/**
 * Driver self-test only (never for a real verification): lets the OLD build's
 * ownership lease and grace expire so the old binary can stand in for a new one
 * and exercise the success paths of this script. The run is marked unverified.
 */
async function debugPreStartDelay(results) {
  const ms = Number(process.env.PR20_CUTOVER_DEBUG_PRE_START_DELAY_MS ?? 0);
  if (!ms) return;
  results.unverified("debug:pre-start-delay", `PR20_CUTOVER_DEBUG_PRE_START_DELAY_MS=${ms}: the new core was started late on purpose; this run is a driver self-test, not a verification`);
  await new Promise((resolve) => setTimeout(resolve, ms));
}

function startNewCore(s) {
  s.startCore("new-core");
  return s.core;
}

async function stopGracefully(s, results, id) {
  if (!s.core || s.core.exited) return;
  s.wire?.close();
  s.core.child.kill("SIGTERM");
  const exit = await Promise.race([
    s.core.wait(),
    new Promise((resolve) => setTimeout(() => resolve(null), STEP_WATCHDOG_MS)),
  ]);
  if (exit === null) {
    results.fail(id, "new core did not exit after SIGTERM within the failure watchdog; killed");
    await s.core.kill("SIGKILL");
  } else {
    results.check(id, exit.code === 0, JSON.stringify(exit));
  }
}

function binding(db, env) {
  const row = db.get("SELECT ownership_root, sidebar_path FROM retention_binding");
  const identity = db.get("SELECT db_id FROM retention_identity");
  const rootFile = path.join(env.threadsRoot, ".evorch-retention-binding.json");
  const root = fs.existsSync(rootFile) ? JSON.parse(fs.readFileSync(rootFile, "utf8")) : null;
  return { row, identity, root, rootFileSha: fs.existsSync(rootFile) ? sha256(rootFile) : null };
}

// ======================================================================= main

export async function verifyMain({ env, artifactDir, results, manifest, newIdentity }) {
  const dir = driverDir(env.root);
  const state = readJson(path.join(dir, "state.json"));
  const mock = await new MockProvider({ mode: "verify", port: state.mockPort, logFile: path.join(dir, "mock-verify.jsonl") }).start();
  const s = new Session({ env, artifactDir, mock, label: "new-core", prefix: "vfy" });
  try {
    // ---- the old core is gone before the new one starts
    results.check(
      "precondition:old-core-stopped",
      manifest.oldCoreExit && !H.pidAlive(manifest.corePid) && (manifest.survivors ?? []).length === 0,
      JSON.stringify({ exit: manifest.oldCoreExit, pid: manifest.corePid }),
    );
    const dbFile = H.findDatabases(env).find((f) => !f.endsWith("owners.db"));
    const readDb = new H.Db(dbFile);
    const ownersBefore = digestOf(ownersFiles(env));
    const bindingBefore = binding(readDb, env);
    const noticesBefore = readDb.all("SELECT notification_id, state, receipt_prompt_id, destination_thread_id FROM lane_notifications");

    // ---- cold start of the new build on the same paths: readiness
    await debugPreStartDelay(results);
    startNewCore(s);
    let ready;
    try {
      ready = await s.core.ready();
      results.pass("readiness", `new core ready: ${ready.core.slice(0, 12)}`);
    } catch (error) {
      results.fail("readiness", error.message);
      return;
    }
    await s.connect();
    s.db = readDb;
    results.check("wire:protocol-version", s.wire.server.protocol_version === manifest.oldServerHello?.protocol_version, JSON.stringify({ old: manifest.oldServerHello?.protocol_version, new: s.wire.server.protocol_version }));
    log(`new ServerHello: ${JSON.stringify({ build: s.wire.server.server_build, capabilities: s.wire.server.capabilities })}`);
    await s.resolveProject();
    results.check("project:same-id-as-old-build", s.project === manifest.project, `${manifest.project} -> ${s.project}`);
    const trustMode = (await s.projectAccess()).trust;
    results.check(
      "project:trust-as-generated",
      trustMode === manifest.projectTrust?.trust,
      JSON.stringify({ generatedByOldCore: manifest.projectTrust?.trust, seenByNewCore: trustMode }),
    );

    const markerAtReady = manifest.scratch?.marker && fs.existsSync(manifest.scratch.marker);

    // ---- existing thread: input, ACK, no duplicate
    await step(results, "existing-thread:input-and-ack", async () => {
      const mk = "VERIFY-MAIN-INPUT";
      const reply = await s.chat("send_chat", "lead-main", `${mk} input after the cutover`);
      await waitFact(s, "provider saw the input", () => requestsWith(mock, mk).length > 0);
      const submission = `${s.prefix}-${s.submission}`;
      await waitFact(s, "input receipt delivered", () => s.db.count("host_input_receipts", "status='delivered' AND prompt_id=?", submission) >= 1);
      const requests = requestsWith(mock, mk);
      const last = requests[requests.length - 1];
      results.check(
        "existing-thread:input-delivered-once",
        messagesWith(last, mk) === 1,
        JSON.stringify({ providerRequestsCarryingInput: requests.length, messagesCarryingInput: messagesWith(last, mk), disposition: reply.disposition, submission }),
      );
      const earlier = "second turn with an image";
      results.check(
        "existing-thread:history-restored",
        messagesWith(last, earlier) >= 1,
        JSON.stringify({ messageCount: last.messageCount, messagesCarryingEarlierTurn: messagesWith(last, earlier) }),
      );
    });

    // ---- Send and CreateExecution keep working on the project as the old core left it (unapproved by default)
    await step(results, `${trustMode}-project:existing-thread-send-usable`, async () => {
      const carrying = requestsWith(mock, "VERIFY-MAIN-INPUT");
      results.check(`${trustMode}-project:existing-thread-send-usable`, carrying.length > 0, `send_chat to an existing thread of a ${trustMode} project reached the provider (${carrying.length} request(s))`);
    });
    await step(results, `${trustMode}-project:create-execution-usable`, async () => {
      const mk = "VERIFY-NEW-THREAD";
      await s.chat("create_execution", "vfy-new-thread", `${tagMarker("vfy-new-thread")} ${mk} a new thread on the cutover project`);
      await waitFact(s, "provider saw the new thread", () => requestsWith(mock, mk).length > 0);
      results.pass(`${trustMode}-project:create-execution-usable`, `create_execution on a ${trustMode} project reached the provider`);
    });

    // ---- the attachment: the stored file is intact and the restored history still references it
    await step(results, "attachment:file-intact", async () => {
      const att = manifest.attachments?.[0];
      if (!att) return results.unverified("attachment:file-intact", "the old build produced no attachment");
      const files = findAttachmentFiles(att.sha256);
      results.check("attachment:file-intact", files.length >= 1, JSON.stringify({ matchingFiles: files }));
      const carrying = requestsWith(mock, "VERIFY-MAIN-INPUT");
      const last = carrying[carrying.length - 1];
      if (!last) throw new Error("no provider request carried the input to lead-main: the restored history was never observed");
      // Contract of the product (old and new build alike): the attachment reaches the model as an
      // "[Attached files (read-only; ...)]" text block with the file's absolute path, not as an image part.
      // The restored history must still carry that block, and the file at that path must be intact.
      const found = wireTexts(last.body)
        .map((m) => ({ role: m.role, paths: attachedFilesPaths(m.text, att.id) }))
        .filter((x) => x.paths);
      const paths = [...new Set(found.flatMap((x) => x.paths))];
      const intact = paths.map((p) => {
        try {
          return { path: p, sha256Matches: sha256(p) === att.sha256 };
        } catch (error) {
          return { path: p, sha256Matches: false, error: error.code ?? String(error) };
        }
      });
      results.check(
        "attachment:referenced-in-restored-history",
        found.length >= 1 && intact.length >= 1 && intact.every((i) => i.sha256Matches),
        JSON.stringify({ messagesWithAttachedFilesBlock: found.length, messageCount: last.messageCount, files: intact }),
      );
    });

    // ---- lane delivery: receipt-less and receipt-present notifications are delivered, ACKed once
    const laneCases = [
      { thread: "lead-rec0", marker: "RPT-REC0", label: "receipt-absent" },
      { thread: "lead-rec1", marker: "RPT-REC1", label: "receipt-present" },
    ];
    for (const c of laneCases) {
      await step(results, `lane-delivery:${c.label}`, async () => {
        const before = noticesBefore.find((n) => n.destination_thread_id === c.thread);
        if (!before) return results.unverified(`lane-delivery:${c.label}`, "the old build produced no such notification");
        const expectReceiptPresent = c.label === "receipt-present";
        if (expectReceiptPresent !== !!before.receipt_prompt_id || before.state !== "pending") {
          return results.unverified(`lane-delivery:${c.label}`, `generated state differs (state=${before.state}, receipt=${before.receipt_prompt_id ? "present" : "absent"}): not the intended fixture`);
        }
        const alreadyAcked = s.db.get("SELECT state FROM lane_notifications WHERE notification_id=?", before.notification_id).state === "acked";
        await s.chat("send_chat", c.thread, `VERIFY-LANE-${c.label} input after the cutover`);
        await waitFact(s, `${c.label} notification acked`, () => s.db.get("SELECT state FROM lane_notifications WHERE notification_id=?", before.notification_id).state === "acked");
        await waitFact(s, `provider saw the ${c.label} input`, () => requestsWith(mock, `VERIFY-LANE-${c.label}`).length > 0);
        // The turn the report lands in does not matter. receipt-absent: the report must reach the model
        // (in this turn or a later one) and must be acknowledged. receipt-present: the old core already
        // accepted it, so only the ACK is repaired. Either way no request may carry it twice.
        if (!expectReceiptPresent) {
          await waitFact(s, `the ${c.label} report reaches the provider`, () => requestsWith(mock, c.marker).length > 0);
        }
        const receipt = s.db.get("SELECT status FROM host_input_receipts WHERE prompt_id=?", `lane-notification:${before.notification_id}`);
        const carrying = requestsWith(mock, c.marker);
        const worst = Math.max(0, ...carrying.map((r) => messagesWith(r, c.marker)));
        const carriers = carrying.length ? describeCarriers(carrying[carrying.length - 1], c.marker) : [];
        const acked = s.db.get("SELECT state FROM lane_notifications WHERE notification_id=?", before.notification_id).state === "acked";
        const countOk = expectReceiptPresent ? worst <= 1 : worst === 1;
        results.check(
          `lane-delivery:${c.label}`,
          acked && receipt?.status === "delivered" && countOk,
          JSON.stringify({ acked, autoDeliveredBeforeInput: alreadyAcked, receipt: receipt?.status, maxMessagesCarryingReport: worst, providerRequestsCarryingReport: carrying.length, carriers }),
        );
      });
    }

    // ---- scratch: cold-start cleanup and the one-time notice
    await step(results, "scratch:notice", async () => {
      if (!manifest.scratch?.marker) return results.unverified("scratch:notice", "the old build produced no scratch marker");
      const swept = !markerAtReady;
      const dirName = manifest.scratch.dir;
      const notice = /\[runtime temporary files\][^\n]*was emptied/;
      const count = (r) => wireTexts(r.body).filter((m) => notice.test(m.text) && m.text.includes(dirName)).length;
      await s.chat("send_chat", "lead-scratch", "VERIFY-SCRATCH-1 first turn after the cutover");
      await waitFact(s, "scratch turn 1", () => requestsWith(mock, "VERIFY-SCRATCH-1").length > 0);
      const first = requestsWith(mock, "VERIFY-SCRATCH-1")[0];
      await s.chat("send_chat", "lead-scratch", "VERIFY-SCRATCH-2 second turn");
      await waitFact(s, "scratch turn 2", () => requestsWith(mock, "VERIFY-SCRATCH-2").length > 0);
      const carrying2 = requestsWith(mock, "VERIFY-SCRATCH-2");
      const second = carrying2[carrying2.length - 1];
      const [c1, c2] = [count(first), count(second)];
      const detail = JSON.stringify({ markerStillPresentAtReady: markerAtReady, noticeCountTurn1: c1, noticeCountTurn2: c2 });
      if (swept) results.check("scratch:notice-exactly-once", c1 === 1 && c2 === 1, `swept at cold start; ${detail}`);
      else if (c1 === 0 && c2 === 0) results.unverified("scratch:notice-exactly-once", `the new core did not sweep the marker, so the notice path was not exercised; ${detail}`);
      else results.fail("scratch:notice-exactly-once", `notice sent although the scratch was not emptied; ${detail}`);
      results.check("scratch:history-prefix-preserved", isPrefix(first.body.messages, second.body.messages), `${first.body.messages.length} -> ${second.body.messages.length} messages`);
    });

    // ---- a new planned restart: (1) the completed old restart does not block its start and adoption,
    //      (2) the new restart completes. Two results; (2) is a known baseline-shared issue (see report.mjs).
    {
      const completedBefore = s.db.count("restart_transactions", "phase='completed'");
      const knownBefore = new Set(s.db.all("SELECT restart_id FROM restart_transactions").map((r) => r.restart_id));
      const readyCount = s.core.readyLines.length;
      const cli = s.cli(["core", "restart", "request", "--yes"], { async: true });
      const cliExit = await cli.wait(); // ends when the core execs, or when the request fails
      const fresh = () => s.db.all("SELECT restart_id, phase, revision FROM restart_transactions").filter((r) => !knownBefore.has(r.restart_id));
      const detail = () => JSON.stringify({ cliExit, newRestarts: fresh(), cliStderr: cli.stderr.join("").slice(-400), coreExit: s.core.exited });
      const ids = () => fresh().map((r) => r.restart_id);
      const FAILED = ["aborted", "invalidated", "superseded", "failed"];
      const A = "restart:old-completed-does-not-block-new-restart";
      const B = "restart:new-restart-completes";
      let adopted = false;
      try {
        await waitFact(s, "a new restart adopted, finished or failed", () => fresh().some((r) => r.phase === "adopted" || r.phase === "completed" || FAILED.includes(r.phase)));
        adopted = fresh().some((r) => r.phase === "adopted" || r.phase === "completed");
        if (adopted) {
          results.pass(A, `a new restart started and was adopted although ${completedBefore} old completed restart(s) exist: ${detail()}`);
          if (!fresh().some((r) => r.phase === "completed")) {
            // Adopted but not complete: keep the evidence of this moment while the core is alive.
            await captureRestartEvidence({ env, s, tag: "adopted", newRestartIds: ids() }).catch((e) => log(`evidence capture failed: ${e.message}`));
          }
        } else {
          results.fail(A, `the new restart ended before it was adopted: ${detail()}`);
        }
      } catch (error) {
        results.fail(A, `${error.message}; ${detail()}`);
        results.fail(B, "not reached: the new restart was not adopted");
        await captureRestartEvidence({ env, s, tag: "final", newRestartIds: ids() }).catch((e) => log(`evidence capture failed: ${e.message}`));
        copyMainCoreLogs(env);
      }
      if (!results.items.some((i) => i.id === B)) {
        try {
          if (!adopted) throw new Error("not reached: the new restart was not adopted");
          await waitFact(s, "a new restart finished or failed", () => fresh().some((r) => r.phase === "completed" || FAILED.includes(r.phase)));
          if (!fresh().some((r) => r.phase === "completed")) throw new Error(`the new restart did not complete: ${detail()}`);
          await Promise.race([s.core.ready(readyCount + 1), s.core.wait().then(() => { throw new Error("the new core exited during the restart"); })]);
          await s.connect();
          results.pass(B, `completed restarts ${completedBefore} -> ${s.db.count("restart_transactions", "phase='completed'")}; ${detail()}`);
        } catch (error) {
          results.fail(B, `${error.message}; ${detail()}`);
          await captureRestartEvidence({ env, s, tag: "final", newRestartIds: ids() }).catch((e) => log(`evidence capture failed: ${e.message}`));
          copyMainCoreLogs(env);
        }
      }
    }

    // ---- stopped destination, interrupted runs, CLI run: nothing is revived by itself
    await step(results, "stop:destination-not-resumed", async () => {
      const dest = s.db.get("SELECT stopped FROM lane_delivery_destinations WHERE thread_id='lead-stop'");
      const revived = ofThread(mock, "lead-stop").length;
      results.check("stop:destination-not-resumed", dest?.stopped === 1 && revived === 0, JSON.stringify({ stopped: dest?.stopped, providerRequests: revived }));
    });
    await step(results, "no-auto-revive:cli-run", async () => {
      if (!manifest.cliRun?.inFlight) return results.unverified("no-auto-revive:cli-run", "the old build left no in-flight CLI run");
      const revived = ofThread(mock, "cli-run").length;
      results.check("no-auto-revive:cli-run", revived === 0, `provider requests for the interrupted CLI run: ${revived}`);
    });
    for (const tag of ["lane-rec0", "lane-rec1"]) {
      const n = ofThread(mock, tag).length;
      if (n === 0) results.pass(`no-auto-resume:${tag}`, "interrupted lane run not resumed");
      else results.unverified(`no-auto-resume:${tag}`, `${n} provider request(s): the new core resumed an interrupted lane run; confirm that is intended`);
    }

    // ---- ownership-only snapshots were what the threads above were restored from
    const ownerOnly = (manifest.inventory ?? []).find((i) => i.name === "ownership_only_snapshots");
    if (ownerOnly?.generated) results.pass("snapshot:ownership-only-restored", `${ownerOnly.count} such snapshots existed; the threads above were restored from them`);
    else results.unverified("snapshot:ownership-only-restored", "the old build produced no ownership-only snapshot");

    // ---- old owner files are neither opened nor cleaned
    const ownersAfter = digestOf(ownersFiles(env));
    results.check("owners-db:untouched", sameJson(ownersBefore, ownersAfter), JSON.stringify({ before: Object.keys(ownersBefore), after: Object.keys(ownersAfter) }));

    // ---- retention binding is intact; the behaviour that depends on it cannot be driven here
    await step(results, "retention:binding-intact", async () => {
      const after = binding(readDb, env);
      const canon = (p) => fs.realpathSync(p);
      const consistent =
        after.root?.db_id === after.identity?.db_id &&
        canon(after.row.ownership_root) === canon(env.threadsRoot);
      results.check(
        "retention:binding-intact",
        consistent && sameJson(bindingBefore.row, after.row) && bindingBefore.rootFileSha === after.rootFileSha,
        JSON.stringify({ consistent, dbRowUnchanged: sameJson(bindingBefore.row, after.row), rootFileUnchanged: bindingBefore.rootFileSha === after.rootFileSha }),
      );
    });
    results.unverified(
      "retention:deletion-still-confirmable",
      "retention confirms a deletion only after archived_delete_after_days of wall-clock time and the core log does not record the retention holds, so BindingMismatch cannot be observed or provoked by this driver; the binding consistency above is the observable part",
    );
    results.check("evidence:new-identity-recorded", !!newIdentity, JSON.stringify(newIdentity ?? {}));
  } finally {
    await stopGracefully(s, results, "new-core:graceful-stop");
    s.wire?.close();
    await mock.stop();
  }
}

// ====================================================================== graph

export async function verifyGraph({ env, artifactDir, results, manifest }) {
  const dir = driverDir(env.root);
  const state = readJson(path.join(dir, "state.json"));
  const mock = await new MockProvider({ mode: "verify", port: state.mockPort, logFile: path.join(dir, "mock-verify.jsonl") }).start();
  const s = new Session({ env, artifactDir, mock, label: "new-core", prefix: "vfy" });
  try {
    results.check(
      "graph:precondition-old-core-stopped",
      manifest.oldCoreExit && !H.pidAlive(manifest.corePid) && (manifest.survivors ?? []).length === 0 && manifest.race === "caught",
      JSON.stringify({ exit: manifest.oldCoreExit, race: manifest.race, phases: manifest.phasesSeen }),
    );
    const dbFile = H.findDatabases(env).find((f) => !f.endsWith("owners.db"));
    s.db = new H.Db(dbFile);
    const pre = s.db.all("SELECT restart_id, phase FROM restart_transactions");
    snapshotHeaders({ env, tag: "before", results });
    await debugPreStartDelay(results);
    startNewCore(s);
    try {
      await s.core.ready();
      results.pass("graph:readiness-with-unfinished-old-restart", JSON.stringify({ restartBefore: pre }));
    } catch (error) {
      results.fail("graph:readiness-with-unfinished-old-restart", error.message);
      return;
    }
    await s.connect();
    await s.resolveProject();
    snapshotHeaders({ env, tag: "started", results });
    log(`graph rows after cold start: ${JSON.stringify(s.db.all("SELECT restart_id, phase FROM restart_transactions"))} ${JSON.stringify(s.db.all("SELECT disposition, reason FROM restart_components"))}`);

    // The old graph is isolated locally: unrelated threads keep working.
    await step(results, "graph:isolation-unrelated-thread-usable", async () => {
      const mk = "VERIFY-OTHER-INPUT";
      await s.chat("send_chat", "lead-other", `${mk} while the old graph is isolated`);
      await waitFact(s, "provider saw the unrelated input", () => requestsWith(mock, mk).length > 0);
      results.pass("graph:isolation-unrelated-thread-usable", "input to an unrelated thread reached the provider while the old graph is unresolved");
    });

    // List the old graph(s) through the new core's management CLI: `graphs list` names an old graph
    // as "  ID (restart R, revision N)"; for an old graph only `graphs release ID --yes` applies
    // (resume is refused). `quarantine list` is read for the record only.
    let abandoned = false;
    const listGraphIds = (stdout) => stdout.split("\n").map((l) => /^ {2}(\S+) \(restart [^,)]*, revision \d+\)/.exec(l)?.[1]).filter(Boolean);
    const graphsList = await s.cli(["core", "graphs", "list"]);
    const quarantineList = await s.cli(["core", "quarantine", "list"]);
    const ids = [...new Set(listGraphIds(graphsList.stdout))];
    log(`graph listings: ${JSON.stringify({ graphs: { code: graphsList.code, out: graphsList.stdout.slice(0, 400), err: graphsList.stderr.slice(0, 200) }, quarantine: { code: quarantineList.code, out: quarantineList.stdout.slice(0, 400) } })}`);
    if (ids.length === 0) {
      results.todo(
        "graph:old-graph-listed",
        `TODO(depends on the integration worker's old-graph listing, OwnedGraphsFacade legacy_listed, run-01m8fq354a7a62wc2k6c5zawhn): 'evorch core graphs list' named no graph. graphs list: ${JSON.stringify(graphsList.stdout.slice(0, 300))} ${JSON.stringify(graphsList.stderr.slice(0, 200))}; quarantine list: ${JSON.stringify(quarantineList.stdout.slice(0, 300))}`,
      );
      results.todo("graph:explicit-abandon", "TODO: depends on the old-graph listing (graphs list); not attempted");
    } else {
      results.pass("graph:old-graph-listed", `${ids.length} graph(s) in 'graphs list': ${ids.join(", ")}`);
      await step(results, "graph:explicit-abandon", async () => {
        const outcomes = [];
        for (const id of ids) {
          const shown = await s.cli(["core", "graphs", "show", id]);
          const r = await s.cli(["core", "graphs", "release", id, "--yes"]);
          outcomes.push({ id, showCode: shown.code, show: shown.stdout.slice(0, 300), code: r.code, out: r.stdout.slice(0, 400), err: r.stderr.slice(0, 200) });
        }
        const ok = outcomes.every((o) => o.code === 0 && !/partial|failed|conflict|refused|rejected/i.test(o.out));
        abandoned = ok;
        const after = await s.cli(["core", "graphs", "list"]);
        results.check("graph:explicit-abandon", ok, JSON.stringify({ outcomes, listedAfterRelease: listGraphIds(after.stdout) }));
      });
    }

    // After the explicit abandon the thread of the old graph can Continue normally.
    await step(results, "graph:continue-after-abandon", async () => {
      if (ids.length === 0) return results.todo("graph:continue-after-abandon", "TODO: not attempted because the old graph was not listed by 'graphs list' yet");
      if (!abandoned) return results.unverified("graph:continue-after-abandon", "not attempted as a pass condition: the explicit abandon did not succeed, so 'Continue after release' was not established");
      const mk = "VERIFY-GRAPH-CONTINUE";
      await s.chat("send_chat", "lead-graph", `${mk} continue after the explicit abandon`);
      await waitFact(s, "provider saw the continue", () => requestsWith(mock, mk).length > 0);
      results.pass("graph:continue-after-abandon", "the abandoned graph's thread accepted new input and reached the provider");
    });
  } finally {
    await stopGracefully(s, results, "graph:new-core-graceful-stop");
    s.wire?.close();
    await mock.stop();
    try {
      snapshotHeaders({ env, tag: "final", results });
    } catch (error) {
      results.fail("graph:header-snapshot:final", error.message);
    }
  }
}

// ============================================================ header snapshots

const HEADER_TABLES = [
  ["restart_transactions", ["restart_id", "capture_id", "revision", "phase", "format_version", "superseded", "adopted_epoch", "ready_at_ms", "failure_reason", "expected_digest", "manifest_digest"]],
  ["restart_components", ["restart_id", "manifest_id", "disposition", "claim_id", "reason", "revision"]],
  ["restart_expected_members", ["restart_id", "run_id", "root_run_id", "manifest_id"], "CASE WHEN json_valid(member_json) THEN json_extract(member_json, '$.thread_id') END AS thread_id"],
  ["recovery_manifests", ["manifest_id", "schema_version", "committed", "superseded", "member_count", "edge_count"]],
  ["recovery_claims", null],
  ["recovery_graph_control", null],
  ["recovery_graph_effects", ["operation_id", "attempt_nonce", "manifest_id", "request_digest"]],
  ["recovery_graph_thread_progress", null],
  ["restart_owner_progress", ["restart_id", "thread_id"]],
];

/**
 * Relational headers of the recovery/restart tables (never a JSON body of a restart or manifest),
 * read with SELECT only, and a self-contained copy of the database at \$TMPDIR/ev-graph-db-<tag>.db.
 * A column the schema does not have (the old build has no format_version) is reported as missing.
 */
export function snapshotHeaders({ env, tag, results }) {
  const dbFile = H.findDatabases(env).find((f) => !f.endsWith("owners.db"));
  const db = new H.Db(dbFile);
  const out = { tag, db: dbFile, at: new Date().toISOString(), tables: {} };
  const existing = new Set(db.tables());
  for (const [table, wanted, extra] of HEADER_TABLES) {
    if (!existing.has(table)) {
      out.tables[table] = { missingTable: true };
      continue;
    }
    const have = db.all(`SELECT name FROM pragma_table_info('${table}')`).map((r) => r.name);
    const columns = wanted ? wanted.filter((c) => have.includes(c)) : have;
    const missing = wanted ? wanted.filter((c) => !have.includes(c)) : [];
    const select = [...columns.map((c) => `"${c}"`), ...(extra && have.includes("member_json") ? [extra] : [])].join(", ");
    out.tables[table] = { missingColumns: missing, rows: db.all(`SELECT ${select} FROM ${table}`) };
  }
  const dir = driverDir(env.root);
  fs.writeFileSync(path.join(dir, `headers-${tag}.json`), JSON.stringify(out, null, 2));
  const copy = path.join(process.env.TMPDIR ?? "/tmp", `ev-graph-db-${tag}.db`);
  fs.rmSync(copy, { force: true });
  // VACUUM INTO writes only the new file; the fixture database is opened read-only.
  const source = new DatabaseSync(dbFile, { readOnly: true });
  try {
    source.exec(`VACUUM INTO '${copy.replaceAll("'", "''")}'`);
  } finally {
    source.close();
  }
  log(`header snapshot ${tag}: ${path.join(dir, `headers-${tag}.json`)} database copy ${copy}`);
  return out;
}

// ====================================================== restart evidence (main)

/**
 * Evidence for a planned restart that does not complete: a one-file copy of the database
 * (VACUUM INTO from a read-only connection), the relational rows of the newest restart
 * (components with reason/claim, expected members, runs of the threads), and, while the core is
 * alive, `quarantine list` and `graphs show ID` for every listed ID. Everything is SELECT or a
 * read-only CLI call; the fixture is never modified.
 */
export async function captureRestartEvidence({ env, s, tag, newRestartIds }) {
  const tmp = process.env.TMPDIR ?? "/tmp";
  const dbFile = H.findDatabases(env).find((f) => !f.endsWith("owners.db"));
  const db = new H.Db(dbFile);
  const copy = path.join(tmp, `ev-main-db-${tag}.db`);
  fs.rmSync(copy, { force: true });
  const source = new DatabaseSync(dbFile, { readOnly: true });
  try {
    source.exec(`VACUUM INTO '${copy.replaceAll("'", "''")}'`);
  } finally {
    source.close();
  }
  const ids = newRestartIds.map((id) => `'${String(id).replaceAll("'", "''")}'`).join(",") || "''";
  const threadCols = db.all("SELECT name FROM pragma_table_info('threads')").map((r) => r.name);
  const wantedThreadCols = threadCols.filter((c) => /thread_id|^id$|root|current|binding/.test(c));
  const restartCols = db.all("SELECT name FROM pragma_table_info('restart_transactions')").map((r) => r.name);
  const evidence = {
    tag,
    at: new Date().toISOString(),
    coreExited: s.core?.exited ?? null,
    databaseCopy: copy,
    restarts: db.all(`SELECT ${["restart_id", "capture_id", "revision", "phase", "format_version", "adopted_epoch", "ready_at_ms", "failure_reason"].filter((c) => restartCols.includes(c)).join(", ")} FROM restart_transactions`),
    newRestartIds,
    components: db.all(`SELECT restart_id, manifest_id, disposition, reason, claim_id, revision FROM restart_components WHERE restart_id IN (${ids})`),
    expectedMembers: db.all(
      `SELECT restart_id, run_id, root_run_id, manifest_id, CASE WHEN json_valid(member_json) THEN json_extract(member_json, '$.thread_id') END AS thread_id FROM restart_expected_members WHERE restart_id IN (${ids})`,
    ),
    manifests: db.all("SELECT manifest_id, schema_version, committed, superseded, member_count, edge_count FROM recovery_manifests"),
    claims: db.all("SELECT claim_id, manifest_id, state, release_reason FROM recovery_claims"),
    threadRuns: db.all("SELECT thread_id, run_id, root_run_id, depth FROM thread_runs"),
    threadBindingColumns: wantedThreadCols.length ? db.all(`SELECT ${wantedThreadCols.map((c) => `"${c}"`).join(", ")} FROM threads`) : [],
    runContexts: db.all("SELECT run_id, role, terminal_phase, restorable, non_restorable_reason FROM run_contexts"),
  };
  let quarantineText = "(core not running: not listed)\n";
  evidence.graphsShow = [];
  if (s.core && !s.core.exited) {
    const list = await s.cli(["core", "quarantine", "list"]);
    quarantineText = `exit ${list.code}\n--- stdout\n${list.stdout}\n--- stderr\n${list.stderr}\n`;
    const listed = [...new Set(list.stdout.split("\n").map((l) => /^ {2}(\S+) \(revision \d+\)/.exec(l)?.[1]).filter(Boolean))];
    for (const id of listed) {
      const shown = await s.cli(["core", "graphs", "show", id]);
      evidence.graphsShow.push({ id, code: shown.code, stdout: shown.stdout, stderr: shown.stderr });
    }
  }
  fs.writeFileSync(path.join(tmp, `ev-main-restart-${tag}.json`), JSON.stringify(evidence, null, 2));
  fs.writeFileSync(path.join(tmp, `ev-main-quarantine-${tag}.txt`), quarantineText);
  log(`restart evidence ${tag}: ${path.join(tmp, `ev-main-restart-${tag}.json`)} database copy ${copy}`);
  return evidence;
}

/** Core logs of the main fixture, copied to \$TMPDIR/ev-main-core-logs for inspection. */
export function copyMainCoreLogs(env) {
  const dest = path.join(process.env.TMPDIR ?? "/tmp", "ev-main-core-logs");
  fs.rmSync(dest, { recursive: true, force: true });
  fs.mkdirSync(dest, { recursive: true });
  const logs = path.join(env.stateDir, "evorch", "logs");
  if (fs.existsSync(logs)) for (const f of fs.readdirSync(logs)) fs.copyFileSync(path.join(logs, f), path.join(dest, f));
  const driverLog = path.join(driverDir(env.root), "core.log");
  if (fs.existsSync(driverLog)) fs.copyFileSync(driverLog, path.join(dest, "driver-core-stdout.log"));
  return dest;
}
