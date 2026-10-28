// 回答 JSON と期待値の照合。使い方: node check-json.mjs <t1|t2> <answer> <expected>
// 不一致を列挙し、1 件でもあれば終了コード 1。
import { readFileSync } from "node:fs";

const [kind, answerPath, expectedPath] = process.argv.slice(2);
let answer;
try {
  answer = JSON.parse(readFileSync(answerPath, "utf8"));
} catch (e) {
  console.error(`FAIL: ${answerPath} を JSON として読めない: ${e.message}`);
  process.exit(1);
}
const expected = JSON.parse(readFileSync(expectedPath, "utf8"));
const problems = [];
let total = 0;

function eq(label, got, want) {
  total++;
  if (got !== want) problems.push(`${label}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
}

if (kind === "t1") {
  if (!Array.isArray(answer)) {
    console.error("FAIL: t1 は JSON 配列でなければならない");
    process.exit(1);
  }
  const byName = new Map(answer.map((e) => [e?.name, e]));
  eq("要素数", answer.length, expected.length);
  eq("name の昇順", JSON.stringify(answer.map((e) => e?.name)), JSON.stringify(expected.map((e) => e.name)));
  for (const want of expected) {
    const got = byName.get(want.name);
    if (!got) {
      total++;
      problems.push(`${want.name}: 欠落`);
      continue;
    }
    for (const key of Object.keys(want)) eq(`${want.name}.${key}`, got[key], want[key]);
  }
  for (const name of byName.keys()) {
    if (!expected.some((e) => e.name === name)) {
      total++;
      problems.push(`${name}: 余計なツール`);
    }
  }
} else if (kind === "t2") {
  const gotCrates = answer?.crates ?? {};
  eq("crate 数", Object.keys(gotCrates).length, Object.keys(expected.crates).length);
  for (const [name, want] of Object.entries(expected.crates)) {
    for (const key of Object.keys(want)) eq(`crates.${name}.${key}`, gotCrates[name]?.[key], want[key]);
  }
  const gotFiles = answer?.largest_files ?? [];
  eq("largest_files 件数", gotFiles.length, expected.largest_files.length);
  expected.largest_files.forEach((want, i) => {
    eq(`largest_files[${i}].path`, gotFiles[i]?.path, want.path);
    eq(`largest_files[${i}].lines`, gotFiles[i]?.lines, want.lines);
  });
} else {
  console.error(`unknown kind ${kind}`);
  process.exit(2);
}

if (problems.length > 0) {
  console.error(`FAIL: ${problems.length}/${total} 項目が不一致`);
  for (const p of problems.slice(0, 30)) console.error(`  - ${p}`);
  if (problems.length > 30) console.error(`  ... ほか ${problems.length - 30} 件`);
  process.exit(1);
}
console.log(`ok: ${total} 項目すべて一致`);
