// t3-rename-v3 の改名検証。使い方: node check-rename-v3.mjs <clone> <base-commit>
//  A. crates/event-bus: 3 つの識別子を、コード行（行頭の空白を除いて `//` で始まらない行）でファイル別に元と同数だけ新名へ（旧名は 0）
//  B. crates/apply-patch の型の定義（フィールド名）が変わり、同名の別のフィールド（AppliedChange.raw）は残っている
//     （使用箇所の網羅と過剰な置換は、コンパイル／テストが保証する）
//  C. event-bus / apply-patch / tools のすべての `.rs` で、コメント行が元と完全に同じ。変更されたファイルはこの 3 crate の中だけ
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const [clone, base] = process.argv.slice(2);
const RENAMES = [
  ["SpanMapper", "SpanTranslator"],
  ["SpanDropKind", "SpanDropReason"],
  ["start_run", "begin_run"],
];
const git = (...args) => execFileSync("git", ["-C", clone, ...args], { maxBuffer: 1 << 28 }).toString();
const tree = (dir) => git("ls-tree", "-r", "--name-only", base, "--", dir).split("\n").filter((f) => f.endsWith(".rs"));
const word = (name) => new RegExp(`\\b${name}\\b`, "g");
const isComment = (line) => line.trimStart().startsWith("//");
const count = (lines, name) => lines.reduce((n, l) => n + (l.match(word(name)) ?? []).length, 0);
const problems = [];

// A.
let sites = 0;
const eventBus = tree("crates/event-bus");
for (const f of eventBus) {
  const baseLines = git("show", `${base}:${f}`).split("\n");
  const baseCode = baseLines.filter((l) => !isComment(l));
  const path = join(clone, f);
  if (!existsSync(path)) {
    problems.push(`${f}: ファイルがない`);
    continue;
  }
  const code = readFileSync(path, "utf8").split("\n").filter((l) => !isComment(l));
  for (const [oldName, newName] of RENAMES) {
    const want = count(baseCode, oldName);
    sites += want;
    const leftover = count(code, oldName);
    const got = count(code, newName);
    if (leftover !== 0) problems.push(`${f}: コード行に旧名 ${oldName} が ${leftover} 件残っている`);
    if (got !== want) problems.push(`${f}: コード行の ${newName} が ${got} 件（期待 ${want}）`);
  }
}

// B.
const block = (text, header) => {
  const a = text.indexOf(header);
  return a < 0 ? "" : text.slice(a, text.indexOf("\n}\n", a));
};
const apply = readFileSync(join(clone, "crates/apply-patch/src/apply.rs"), "utf8");
const targets = readFileSync(join(clone, "crates/apply-patch/src/targets.rs"), "utf8");
const defs = [
  ["AppliedChange", apply, "pub struct AppliedChange {", [/\bpub final_path:/, /\bpub raw:/], [/\bpub path:/]],
  ["CommitFailure", apply, "pub struct CommitFailure {", [/\bpub persisted:/], [/\bpub committed:/]],
  ["ChangeKind", apply, "pub enum ChangeKind {", [/\bbefore: String/, /\bafter: String/], [/\bold: String/, /\bnew: String/]],
  ["PatchTarget", targets, "pub struct PatchTarget {", [/\bpub as_written:/, /\bpub resolved:/], [/\bpub raw:/]],
];
for (const [name, text, header, must, mustNot] of defs) {
  const b = block(text, header);
  if (!b) problems.push(`${name}: 定義が見つからない`);
  for (const re of must) if (!re.test(b)) problems.push(`${name}: 定義に ${re} がない`);
  for (const re of mustNot) if (re.test(b)) problems.push(`${name}: 定義に ${re} が残っている`);
}

// C.
const scope = ["crates/event-bus", "crates/apply-patch", "crates/tools"];
const scopeFiles = scope.flatMap(tree);
for (const f of scopeFiles) {
  const path = join(clone, f);
  if (!existsSync(path)) {
    problems.push(`${f}: ファイルがない`);
    continue;
  }
  const baseComments = git("show", `${base}:${f}`).split("\n").filter(isComment);
  const comments = readFileSync(path, "utf8").split("\n").filter(isComment);
  if (JSON.stringify(comments) !== JSON.stringify(baseComments)) problems.push(`${f}: コメント行が変更されている`);
}
const changed = git("diff", "--name-only", base).split("\n").filter(Boolean);
for (const f of changed) if (!scopeFiles.includes(f)) problems.push(`${f}: 範囲外のファイルが変更されている`);

if (problems.length > 0) {
  console.error(`FAIL: ${problems.length} 件の不一致`);
  for (const p of problems.slice(0, 30)) console.error(`  - ${p}`);
  if (problems.length > 30) console.error(`  ... ほか ${problems.length - 30} 件`);
  process.exit(1);
}
console.log(`ok: event-bus のコード行 ${sites} 箇所を改名、apply-patch の 4 つの定義、コメント行は不変（変更ファイル ${changed.length}）`);
