// t2-aggregate の期待値を、固定コミットの checkout から再計算する。
// 使い方: node scripts/gen-t2-expected.mjs <base-checkout> > verify/expected/t2-aggregate.json
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

const root = process.argv[2];
if (!root) {
  console.error("usage: gen-t2-expected.mjs <base-checkout>");
  process.exit(2);
}
const files = execFileSync("git", ["-C", root, "ls-files", "-z", "--", "crates"], {
  maxBuffer: 1 << 28,
})
  .toString()
  .split("\0")
  .filter((f) => f.endsWith(".rs"));

const crates = {};
const all = [];
for (const f of files) {
  const name = f.split("/")[1];
  const text = readFileSync(`${root}/${f}`, "utf8");
  const lines = (text.match(/\n/g) ?? []).length;
  const tests = text.split("\n").filter((l) => /^\s*#\[(test\]|tokio::test)/.test(l)).length;
  const c = (crates[name] ??= { rs_files: 0, rs_lines: 0, test_attrs: 0 });
  c.rs_files++;
  c.rs_lines += lines;
  c.test_attrs += tests;
  all.push({ path: f, lines });
}
all.sort((a, b) => b.lines - a.lines || a.path.localeCompare(b.path));
const sorted = Object.fromEntries(Object.keys(crates).sort().map((k) => [k, crates[k]]));
console.log(JSON.stringify({ crates: sorted, largest_files: all.slice(0, 5) }, null, 1));
