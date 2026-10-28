#!/usr/bin/env node
// 全 run のトップレベル shell コマンドを「プログラム的構造」で分類し、モデル別の件数と該当コマンドを出す。
// metrics.mjs の analyzeShell と同じ定義（README の「ワンライナーの複雑さ」）。予備実行の目視確認用。
//
// 使い方: classify-shell.mjs --results <outroot> [--list] [--all] [--include-attempts] [--evorch <bin>]
//   --list   プログラム的構造・build 連結のコマンドを本文つきで列挙する（本文は 400 文字まで）
//   --all    --list でプログラム的でないコマンドも含める
import { analyzeShell, loadResults, toolEvents } from './metrics.mjs';

const argv = process.argv.slice(2);
const opt = (name) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : undefined;
};
const root = opt('--results');
if (!root) {
  console.error('usage: classify-shell.mjs --results <outroot> [--list] [--all] [--include-attempts] [--evorch <bin>]');
  process.exit(2);
}
const entries = loadResults(root, opt('--evorch') ?? process.env.EVORCH_BIN, { includeAttempts: argv.includes('--include-attempts') });
for (const msg of entries.skipped) console.error(`skip ${msg}`);

const rows = [];
for (const entry of entries) {
  const doc = entry.events && typeof entry.events === 'object' && 'result' in entry.events ? entry.events.result : entry.events;
  const starts = toolEvents(doc?.events).filter((e) => e.type === 'ToolStarted' && !e.parent_call_id && e.tool_name === 'shell');
  for (const e of starts) {
    const command = e.input?.command;
    if (typeof command !== 'string' || !(e.input.action == null || e.input.action === 'start')) continue;
    rows.push({ model: entry.label.model, task: entry.label.task, rep: entry.label.rep, command, a: analyzeShell(command) });
  }
}

const table = {};
for (const r of rows) {
  const t = (table[r.model] ??= { cmds: 0, prog: 0, script: 0, loop: 0, textproc: 0, sed_multi: 0, build_chain: 0, segments_4plus: 0 });
  t.cmds++;
  const p = r.a.programmatic;
  if (Object.values(p).some(Boolean)) t.prog++;
  for (const k of ['script', 'loop', 'textproc', 'sed_multi']) if (p[k]) t[k]++;
  if (r.a.build_steps >= 2) t.build_chain++;
  if (r.a.segments >= 4) t.segments_4plus++;
}
console.table(table);

if (argv.includes('--list')) {
  const all = argv.includes('--all');
  for (const r of rows) {
    const p = r.a.programmatic;
    const kinds = Object.entries(p).filter(([, v]) => v).map(([k]) => k);
    if (!all && kinds.length === 0 && r.a.build_steps < 2) continue;
    const flags = [...kinds, ...(r.a.build_steps >= 2 ? [`build×${r.a.build_steps}`] : [])].join(',');
    console.log(`[${r.model}/${r.task}/${r.rep}] chars=${r.a.chars} segments=${r.a.segments} {${flags}}\n  ${r.command.replace(/\n/g, '\n  ').slice(0, 400)}`);
  }
}
