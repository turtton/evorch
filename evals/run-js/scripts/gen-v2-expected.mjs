// v2 課題（t1-survey-v2 / t2-aggregate-v2）の期待値を、固定コミットの checkout から再計算する。
// 使い方:
//   node scripts/gen-v2-expected.mjs <base-checkout> <out-dir>
//     <out-dir>/t1-survey-v2.json と t2-aggregate-v2.json を書く（通常は verify/expected/）。
// t1 の name / execution_mode / 権限は v1 の期待値（verify/expected/t1-survey.json。ソースを読んで確定済み）を
// 引き継ぎ、allowed_roles は `Role::capabilities()` を実際に呼んで得た行列（下の ROLE_MATRIX）を使う。
// 参照数・集計は git ls-files と本文の走査で求める（rg / wc による別経路の突き合わせは README 参照）。
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = process.argv[2];
const outDir = process.argv[3];
if (!root || !outDir) {
  console.error("usage: gen-v2-expected.mjs <base-checkout> <out-dir>");
  process.exit(2);
}
const here = dirname(fileURLToPath(import.meta.url));
const tracked = execFileSync("git", ["-C", root, "ls-files", "-z"], { maxBuffer: 1 << 28 })
  .toString()
  .split("\0")
  .filter((f) => f.endsWith(".rs"));
const text = new Map();
const read = (f) => {
  if (!text.has(f)) text.set(f, readFileSync(join(root, f), "utf8"));
  return text.get(f);
};

// ---------------------------------------------------------------- t1-survey-v2
const ROLES = ["Orchestrator", "Explorer", "Worker", "Reviewer", "WebResearcher", "Planner", "Advisor", "MultimodalLooker", "Evaluator"];
// `Role::capabilities().allowed_tools` に各ツールを含むロール（enum の宣言順）。cargo test で出力して確認した値。
const ROLE_MATRIX = {
  apply_patch: ["Worker"],
  edit: ["Worker"],
  git_diff: ["Orchestrator", "Worker", "Reviewer", "Planner", "Advisor"],
  grep: ["Orchestrator", "Explorer", "Worker", "Reviewer", "WebResearcher", "Planner", "Advisor", "Evaluator"],
  read: ROLES,
  shell: ["Worker"],
  web_fetch: ["Orchestrator", "WebResearcher", "Planner"],
  web_search: ["WebResearcher"],
  write: ["Worker"],
};
const v1 = JSON.parse(readFileSync(join(here, "../verify/expected/t1-survey.json"), "utf8"));
const countFiles = (dir, name) =>
  tracked.filter((f) => f.startsWith(dir) && read(f).includes(`"${name}"`)).length;
const tools = v1.map((t) => {
  const refs = {
    tools_tests: countFiles("crates/tools/tests/", t.name),
    runtime_src: countFiles("crates/runtime/src/", t.name),
    runtime_tests: countFiles("crates/runtime/tests/", t.name),
  };
  return {
    name: t.name,
    execution_mode: t.execution_mode,
    fs_read: t.fs_read,
    fs_write: t.fs_write,
    process_spawn: t.process_spawn,
    network: t.network,
    allowed_roles: ROLE_MATRIX[t.name],
    refs,
  };
});
const by_role = Object.fromEntries(
  ROLES.map((r) => [r, tools.filter((t) => t.allowed_roles.includes(r)).map((t) => t.name).sort()]),
);
const total = (t) => t.refs.tools_tests + t.refs.runtime_src + t.refs.runtime_tests;
const ref_ranking = [...tools].sort((a, b) => total(b) - total(a) || a.name.localeCompare(b.name)).map((t) => t.name);
writeFileSync(join(outDir, "t1-survey-v2.json"), JSON.stringify({ tools, by_role, ref_ranking }, null, 1) + "\n");

// ---------------------------------------------------------------- t2-aggregate-v2
const crates = {};
const bases = {};
for (const f of tracked.filter((f) => f.startsWith("crates/"))) {
  const parts = f.split("/");
  const name = parts[1];
  const cat = parts[2] === "src" ? "src" : parts[2] === "tests" ? "tests" : "other";
  const body = read(f);
  const lines = (body.match(/\n/g) ?? []).length;
  const tests = body.split("\n").filter((l) => /^\s*#\[(test\]|tokio::test)/.test(l)).length;
  const c = (crates[name] ??= { rs_files: 0, rs_lines: 0, test_attrs: 0, src_lines: 0, tests_lines: 0, other_lines: 0 });
  c.rs_files++;
  c.rs_lines += lines;
  c.test_attrs += tests;
  c[`${cat}_lines`] += lines;
  const base = parts[parts.length - 1];
  const b = (bases[base] ??= { crates: new Set(), file_count: 0, total_lines: 0 });
  b.crates.add(name);
  b.file_count++;
  b.total_lines += lines;
}
const sortedCrates = Object.fromEntries(Object.keys(crates).sort().map((k) => [k, crates[k]]));
const density = Object.entries(sortedCrates)
  .filter(([, v]) => v.rs_lines >= 8000)
  .map(([name, v]) => ({ crate: name, per_1k: Math.round((v.test_attrs * 100000) / v.rs_lines) / 100 }))
  .sort((a, b) => b.per_1k - a.per_1k || a.crate.localeCompare(b.crate))
  .slice(0, 5);
const EXCLUDED = new Set(["lib.rs", "mod.rs", "main.rs", "build.rs"]);
const shared = Object.entries(bases)
  .filter(([name, v]) => v.crates.size >= 5 && !EXCLUDED.has(name))
  .map(([name, v]) => ({ name, crate_count: v.crates.size, file_count: v.file_count, total_lines: v.total_lines }))
  .sort((a, b) => b.total_lines - a.total_lines || a.name.localeCompare(b.name));
writeFileSync(
  join(outDir, "t2-aggregate-v2.json"),
  JSON.stringify({ crates: sortedCrates, density_top5: density, shared_basenames: shared }, null, 1) + "\n",
);
