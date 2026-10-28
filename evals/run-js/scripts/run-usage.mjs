#!/usr/bin/env node
// run-one.sh の watchdog が使う: `evorch inspect run <id>` の出力（標準入力）から、その run のリクエスト数と費用を
// タブ区切りの 1 行で出す: <requests>\t<cost_usd>
//   requests  usage_requests のうち purpose が agent（または欠落）の行数（metrics.mjs の `requests` と同じ定義）
//   cost_usd  usage_requests 全行の cost_usd の合計（価格不明の行は 0）
// 読めなければ何も出さず終了コード 1。
import fs from 'node:fs';

let doc;
try {
  doc = JSON.parse(fs.readFileSync(0, 'utf8'));
} catch {
  process.exit(1);
}
const rows = (doc?.result ?? doc)?.usage_requests;
if (!Array.isArray(rows)) process.exit(1);
const requests = rows.filter((r) => r.purpose == null || r.purpose === '' || r.purpose === 'agent').length;
const cost = rows.reduce((a, r) => a + (typeof r.cost_usd === 'number' ? r.cost_usd : 0), 0);
console.log(`${requests}\t${cost.toFixed(6)}`);
