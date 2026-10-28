#!/usr/bin/env node
// run-one.sh の watchdog が使う: `evorch inspect events --run <id> --limit N` の出力（標準入力）から、run の末尾の状態を
// タブ区切りの 1 行で出す: <last_id>\t<last_kind>\t<activity>\t<state>\t<text>\t<repeat>
//   last_kind  最後のイベントの種類（TurnCompleted / AgentRunStateChanged / MessageDelta ...）
//   activity   最後の RunProgress の activity（Idle / Tools / User ...）。無ければ -
//   state      最後の AgentRunStateChanged の遷移先（Running / Waiting / Done ...）。無ければ -
//   text       最後の MessageCompleted の本文（タブ・改行を空白にして 200 文字まで。モデルの最後の発言）。無ければ空
//   repeat     末尾から数えて、空でない MessageCompleted の本文が同一のまま何件続いているか（ツールだけの応答は数えない）。
//              同じ応答を繰り返し続ける暴走（完了後の通知に同じ返事をし続ける）の検出に使う
// 読めなければ何も出さず終了コード 1。
import fs from 'node:fs';

let doc;
try {
  doc = JSON.parse(fs.readFileSync(0, 'utf8'));
} catch {
  process.exit(1);
}
const events = (doc?.result ?? doc)?.events;
if (!Array.isArray(events) || events.length === 0) process.exit(1);

const inner = (event) => {
  let cur = event?.payload;
  while (cur && typeof cur === 'object') {
    if (cur.payload && typeof cur.payload === 'object' && 'kind' in cur.payload) cur = cur.payload;
    else return { kind: cur.kind ?? null, data: cur.payload ?? {} };
  }
  return { kind: null, data: {} };
};
const last = events[events.length - 1];
let activity = '-';
let state = '-';
let text = '';
for (const event of events) {
  const { kind, data } = inner(event);
  if (kind === 'RunProgress' && data.activity) activity = data.activity;
  if (kind === 'AgentRunStateChanged' && data.to) state = data.to;
  if (kind === 'MessageCompleted' && typeof data.text === 'string') text = data.text;
}
const clean = text.replace(/[\t\r\n]+/g, ' ').trim().slice(0, 200);
const texts = events.map((e) => inner(e)).filter((x) => x.kind === 'MessageCompleted' && typeof x.data.text === 'string' && x.data.text.trim() !== '').map((x) => x.data.text);
let repeat = 0;
for (let i = texts.length - 1; i >= 0 && texts[i] === texts[texts.length - 1]; i--) repeat++;
console.log([last.id, inner(last).kind ?? '-', activity, state, clean, repeat].join('\t'));
