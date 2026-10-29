// t3-rename-v2 の改名検証。使い方: node check-rename-v2.mjs <clone> <base-commit>
// 固定コミットの crates/event-bus/**/*.rs と clone の同じファイルを比べ、次を確かめる。
//  - コード行（行頭の空白を除いて `//` で始まらない行）: 旧名の単語一致が 0、新名の単語一致が元の旧名と同数（ファイル別）
//  - コメント行（`//` `///` `//!` で始まる行）: 元と完全に同じ（順序も）
//  - `str_attribute`（`str_attr` の部分一致で別の識別子）の個数が不変
//  - 変更されたファイルは、コード行に旧名があったファイルだけ
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const [clone, base] = process.argv.slice(2);
const RENAMES = [
  ["SpanKey", "SpanRef"],
  ["SpanAction", "SpanCommand"],
  ["drain_drops", "take_drops"],
  ["str_attr", "string_attr"],
];
const git = (...args) => execFileSync("git", ["-C", clone, ...args], { maxBuffer: 1 << 28 }).toString();
const files = git("ls-tree", "-r", "--name-only", base, "--", "crates/event-bus").split("\n").filter((f) => f.endsWith(".rs"));
const word = (name) => new RegExp(`\\b${name}\\b`, "g");
const isComment = (line) => line.trimStart().startsWith("//");
const count = (lines, name) => lines.reduce((n, l) => n + (l.match(word(name)) ?? []).length, 0);

const problems = [];
const touched = [];
let withOccurrences = 0;
let totalNew = 0;
for (const f of files) {
  const baseText = git("show", `${base}:${f}`);
  const baseLines = baseText.split("\n");
  const baseCode = baseLines.filter((l) => !isComment(l));
  const baseComments = baseLines.filter(isComment);
  const path = join(clone, f);
  if (!existsSync(path)) {
    problems.push(`${f}: ファイルがない`);
    continue;
  }
  const text = readFileSync(path, "utf8");
  const lines = text.split("\n");
  const code = lines.filter((l) => !isComment(l));
  const comments = lines.filter(isComment);
  const hasOld = RENAMES.some(([o]) => count(baseCode, o) > 0);
  if (hasOld) withOccurrences++;
  if (text !== baseText) touched.push(f);
  if (!hasOld && text !== baseText) problems.push(`${f}: 改名対象でないファイルが変更されている`);
  for (const [oldName, newName] of RENAMES) {
    const want = count(baseCode, oldName);
    const leftover = count(code, oldName);
    const got = count(code, newName);
    totalNew += want;
    if (leftover !== 0) problems.push(`${f}: コード行に旧名 ${oldName} が ${leftover} 件残っている`);
    if (got !== want) problems.push(`${f}: コード行の ${newName} が ${got} 件（期待 ${want}）`);
  }
  if (count(code, "str_attribute") !== count(baseCode, "str_attribute")) problems.push(`${f}: str_attribute（別の識別子）の個数が変わっている`);
  if (JSON.stringify(comments) !== JSON.stringify(baseComments)) problems.push(`${f}: コメント行が変更されている`);
}
// 範囲外のファイル（event-bus 以外、または .rs 以外）が変更されていないこと。
const changed = git("diff", "--name-only", base).split("\n").filter(Boolean);
for (const f of changed) if (!files.includes(f)) problems.push(`${f}: 範囲外のファイルが変更されている`);

if (problems.length > 0) {
  console.error(`FAIL: ${problems.length} 件の不一致`);
  for (const p of problems.slice(0, 30)) console.error(`  - ${p}`);
  if (problems.length > 30) console.error(`  ... ほか ${problems.length - 30} 件`);
  process.exit(1);
}
console.log(`ok: ${withOccurrences} ファイル・コード行の ${totalNew} 箇所を改名、コメント行は不変（変更ファイル ${touched.length}）`);
