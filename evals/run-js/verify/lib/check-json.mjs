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
} else if (kind === "t1v2") {
  const ser = (x) => JSON.stringify(x);
  const gotTools = Array.isArray(answer?.tools) ? answer.tools : [];
  const byName = new Map(gotTools.map((e) => [e?.name, e]));
  eq("tools の要素数", gotTools.length, expected.tools.length);
  eq("tools の name 昇順", ser(gotTools.map((e) => e?.name)), ser(expected.tools.map((e) => e.name)));
  for (const want of expected.tools) {
    const got = byName.get(want.name);
    if (!got) {
      total++;
      problems.push(`${want.name}: 欠落`);
      continue;
    }
    for (const key of ["execution_mode", "fs_read", "fs_write", "process_spawn", "network"]) eq(`${want.name}.${key}`, got[key], want[key]);
    eq(`${want.name}.allowed_roles`, ser(got.allowed_roles), ser(want.allowed_roles));
    for (const key of Object.keys(want.refs)) eq(`${want.name}.refs.${key}`, got.refs?.[key], want.refs[key]);
  }
  for (const [role, want] of Object.entries(expected.by_role)) eq(`by_role.${role}`, ser(answer?.by_role?.[role]), ser(want));
  eq("by_role のロール数", Object.keys(answer?.by_role ?? {}).length, Object.keys(expected.by_role).length);
  eq("ref_ranking", ser(answer?.ref_ranking), ser(expected.ref_ranking));
} else if (kind === "t2v2") {
  const ser = (x) => JSON.stringify(x);
  const gotCrates = answer?.crates ?? {};
  eq("crate 数", Object.keys(gotCrates).length, Object.keys(expected.crates).length);
  for (const [name, want] of Object.entries(expected.crates)) {
    for (const key of Object.keys(want)) eq(`crates.${name}.${key}`, gotCrates[name]?.[key], want[key]);
  }
  const gotDensity = Array.isArray(answer?.density_top5) ? answer.density_top5 : [];
  eq("density_top5 件数", gotDensity.length, expected.density_top5.length);
  expected.density_top5.forEach((want, i) => {
    eq(`density_top5[${i}].crate`, gotDensity[i]?.crate, want.crate);
    total++;
    const g = gotDensity[i]?.per_1k;
    if (typeof g !== "number" || Math.abs(g - want.per_1k) > 0.0101) problems.push(`density_top5[${i}].per_1k: got ${JSON.stringify(g)}, want ${want.per_1k}（±0.01）`);
  });
  const gotShared = Array.isArray(answer?.shared_basenames) ? answer.shared_basenames : [];
  eq("shared_basenames 件数", gotShared.length, expected.shared_basenames.length);
  expected.shared_basenames.forEach((want, i) => {
    for (const key of Object.keys(want)) eq(`shared_basenames[${i}].${key}`, gotShared[i]?.[key], want[key]);
  });
} else if (kind === "t1v3") {
  const ser = (x) => JSON.stringify(x);
  const gotTools = Array.isArray(answer?.tools) ? answer.tools : [];
  const byName = new Map(gotTools.map((e) => [e?.name, e]));
  eq("tools の要素数", gotTools.length, expected.tools.length);
  eq("tools の name 昇順", ser(gotTools.map((e) => e?.name)), ser(expected.tools.map((e) => e.name)));
  for (const want of expected.tools) {
    const got = byName.get(want.name);
    if (!got) {
      total++;
      problems.push(`${want.name}: 欠落`);
      continue;
    }
    for (const key of ["source", "execution_mode", "fs_read", "fs_write", "process_spawn", "network"]) eq(`${want.name}.${key}`, got[key], want[key]);
    eq(`${want.name}.allowed_roles`, ser(got.allowed_roles), ser(want.allowed_roles));
    eq(`${want.name}.overrides`, ser(got.overrides), ser(want.overrides));
    for (const key of Object.keys(want.refs)) eq(`${want.name}.refs.${key}`, got.refs?.[key], want.refs[key]);
  }
  for (const [role, want] of Object.entries(expected.by_role)) eq(`by_role.${role}`, ser(answer?.by_role?.[role]), ser(want));
  eq("by_role のロール数", Object.keys(answer?.by_role ?? {}).length, Object.keys(expected.by_role).length);
  eq("ref_ranking", ser(answer?.ref_ranking), ser(expected.ref_ranking));
} else if (kind === "t2v3") {
  const ser = (x) => JSON.stringify(x);
  const gotCrates = answer?.crates ?? {};
  eq("crates の数", Object.keys(gotCrates).length, Object.keys(expected.crates).length);
  for (const [name, want] of Object.entries(expected.crates)) {
    for (const key of Object.keys(want)) eq(`crates.${name}.${key}`, gotCrates[name]?.[key], want[key]);
  }
  for (const [name, want] of Object.entries(expected.graph)) {
    for (const key of ["deps", "dev_deps"]) eq(`graph.${name}.${key}`, ser(answer?.graph?.[name]?.[key]), ser(want[key]));
    for (const key of ["dependents", "transitive_deps", "layer"]) eq(`graph.${name}.${key}`, answer?.graph?.[name]?.[key], want[key]);
  }
  for (const [name, want] of Object.entries(expected.history)) {
    for (const key of Object.keys(want)) eq(`history.${name}.${key}`, answer?.history?.[name]?.[key], want[key]);
  }
  eq("adr.files", answer?.adr?.files, expected.adr.files);
  eq("adr.status_counts", ser(answer?.adr?.status_counts), ser(expected.adr.status_counts));
  eq("adr.most_referenced", ser(answer?.adr?.most_referenced), ser(expected.adr.most_referenced));
  for (const [top, cells] of Object.entries(expected.ext_matrix)) {
    eq(`ext_matrix.${top} の拡張子の集合`, ser(Object.keys(answer?.ext_matrix?.[top] ?? {}).sort()), ser(Object.keys(cells).sort()));
    for (const [ext, want] of Object.entries(cells)) {
      eq(`ext_matrix.${top}.${ext}.files`, answer?.ext_matrix?.[top]?.[ext]?.files, want.files);
      eq(`ext_matrix.${top}.${ext}.lines`, answer?.ext_matrix?.[top]?.[ext]?.lines, want.lines);
    }
  }
  const gotHot = Array.isArray(answer?.hotspots) ? answer.hotspots : [];
  eq("hotspots 件数", gotHot.length, expected.hotspots.length);
  expected.hotspots.forEach((want, i) => {
    for (const key of ["crate", "commits_200", "dependents", "rs_lines"]) eq(`hotspots[${i}].${key}`, gotHot[i]?.[key], want[key]);
    total++;
    const g = gotHot[i]?.score;
    if (typeof g !== "number" || Math.abs(g - want.score) > 0.0101) problems.push(`hotspots[${i}].score: got ${JSON.stringify(g)}, want ${want.score}（±0.01）`);
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
