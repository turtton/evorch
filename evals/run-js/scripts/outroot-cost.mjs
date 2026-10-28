#!/usr/bin/env node
// 結果ディレクトリ（run-matrix.sh の --outroot）の累計費用を出す: <total_usd>\t<runs>
// 全 run の inspect-run.json（usage_requests の cost_usd。退避された <rep>.attempt<N> も含む。打ち切られた run の分も）の合計。
// 使い方: outroot-cost.mjs <outroot>
import fs from 'node:fs';
import path from 'node:path';

const root = process.argv[2];
if (!root || !fs.existsSync(root)) {
  console.error('usage: outroot-cost.mjs <outroot>');
  process.exit(2);
}
const dirs = (p) => {
  try {
    return fs.readdirSync(p, { withFileTypes: true }).filter((d) => d.isDirectory() && !d.name.startsWith('.')).map((d) => path.join(p, d.name));
  } catch {
    return [];
  }
};
let total = 0;
let runs = 0;
for (const m of dirs(root)) {
  for (const t of dirs(m)) {
    for (const r of dirs(t)) {
      const file = path.join(r, 'inspect-run.json');
      if (!fs.existsSync(file)) continue;
      try {
        const doc = JSON.parse(fs.readFileSync(file, 'utf8'));
        for (const row of (doc.result ?? doc).usage_requests ?? []) total += typeof row.cost_usd === 'number' ? row.cost_usd : 0;
        runs++;
      } catch {
        // 読めない run は数えない
      }
    }
  }
}
console.log(`${total.toFixed(4)}\t${runs}`);
