#!/usr/bin/env node
// Entry of the PR 20 cutover driver. `scripts/verify-pr20-cutover.sh` is the
// supported way to run it; see that file's usage.
import path from "node:path";
import { Results, driverDir, log, readJson, setLogFile } from "./common.mjs";
import { Env } from "./harness.mjs";
import { generateGraph, generateMain } from "./gen.mjs";
import { snapshotHeaders, verifyGraph, verifyMain } from "./verify.mjs";

function parse(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) throw new Error(`unexpected argument ${a}`);
    out[a.slice(2)] = argv[++i];
  }
  return out;
}

const args = parse(process.argv.slice(2));
const phase = args.phase;
const root = args.root;
if (!phase || !root) throw new Error("usage: driver.mjs --phase gen|verify --root DIR --artifact DIR [--fixture main|graph] [--identity FILE] [--approve yes]");
const env = Env.open(root);
setLogFile(path.join(driverDir(root), `${phase}.log`));
const results = new Results(path.join(driverDir(root), `${phase}-results.json`));
try {
  if (phase === "gen") {
    const generate = args.fixture === "graph" ? generateGraph : generateMain;
    const manifest = await generate({ env, artifactDir: args.artifact, results, approve: args.approve === "yes" });
    if (manifest.race === "completed-first") process.exitCode = 3; // the caller retries with a fresh root
    for (const item of manifest.inventory) {
      if (item.generated) results.pass(`generated:${item.name}`, `count=${item.count}`);
      else results.unverified(`generated:${item.name}`, "the old build did not produce this (generated missing)");
    }
  } else if (phase === "snapshot") {
    // Standalone: headers + database copy of an already generated fixture (no new core needed).
    snapshotHeaders({ env, tag: args.tag ?? "before", results });
  } else if (phase === "verify") {
    const fixture = args.fixture === "graph" ? "graph" : "main";
    const manifest = readJson(path.join(driverDir(root), `fixture-${fixture}.json`));
    const newIdentity = args.identity ? readJson(args.identity) : null;
    const run = fixture === "graph" ? verifyGraph : verifyMain;
    await run({ env, artifactDir: args.artifact, results, manifest, newIdentity });
    // What the OLD build could not generate stays unverified in the report, whatever the new build does.
    for (const item of manifest.inventory ?? []) {
      if (!item.generated) results.unverified(`not-generated:${item.name}`, "the old build did not produce this data: the cutover of it was NOT verified");
    }
  } else {
    throw new Error(`unknown phase ${phase}`);
  }
} catch (error) {
  results.fail("driver", error.stack ?? String(error));
  process.exitCode = 1;
}
log(`summary ${JSON.stringify(results.summary())}`);
process.exit(process.exitCode ?? 0);
