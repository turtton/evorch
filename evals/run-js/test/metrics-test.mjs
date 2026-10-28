// metrics.mjs の確認: fixture-metrics.json の期待値。実行: node test/metrics-test.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';
import { aggregate, computeRun, loadResults, outcomeOf, toMarkdown } from '../scripts/metrics.mjs';

const fixture = JSON.parse(fs.readFileSync(fileURLToPath(new URL('./fixture-metrics.json', import.meta.url)), 'utf8'));
const [r1, r2, r3] = fixture.runs.map(computeRun);

// run 1
assert.equal(r1.usage.requests, 6); // purpose 欠落 (null) は agent 扱い
assert.equal(r1.usage.aux_requests, 1);
assert.equal(r1.usage.input_tokens, 21000);
assert.equal(r1.usage.cache_read_tokens, 12000);
assert.equal(r1.usage.cache_write_tokens, 500);
assert.equal(r1.usage.reasoning_tokens, 300);
assert.ok(Math.abs(r1.usage.cost_usd - 0.06) < 1e-9 && Math.abs(r1.usage.aux_cost_usd - 0.001) < 1e-9);
assert.equal(r1.tools.top_level_calls, 7);
assert.deepEqual(r1.tools.by_name, { read: 1, run_js: 3, shell: 3 });
assert.equal(r1.run_js.calls, 3);
assert.equal(r1.run_js.child_calls, 6);
assert.deepEqual(r1.tools.child_by_tool, { grep: 1, read: 3, shell: 1 });
assert.deepEqual(r1.run_js.errors, { js_exception: 1, host_rejected: 1, child_tool_error: 1, timeout: 0, limit_hit: 0, pre_run_error: 0 });
assert.equal(r1.run_js.output_bytes, 300 + 120 + 120);
assert.equal(r1.run_js.return_bytes, 2000 + 40 + 40);
assert.equal(r1.shell.commands, 2);
assert.equal(r1.shell.control_calls, 1);
assert.equal(r1.shell.heredoc_cmds, 1);
assert.deepEqual(r1.shell.interp_inline_by_lang, { python: 1 });
assert.equal(r1.shell.pipes.max, 3);
assert.equal(r1.duration_s, 95);
assert.equal(r1.verify, 'pass');

// 所要時間の fallback（result.json なし）はツール以外も含む全イベントの最初〜最後
const spanEntry = {
  run: { run_id: 'run-span', usage_requests: [], context: {} },
  events: { events: [
    { id: 1, kind: 'Lifecycle', payload: { kind: 'Lifecycle', payload: { kind: 'AgentRunStarted', payload: {} } }, wall_clock_ns: 1000000000 },
    { id: 2, kind: 'Tool', payload: { kind: 'Tool', payload: { kind: 'ToolStarted', payload: { call_id: 'c1', tool_name: 'read', run_id: 'run-span' } } }, wall_clock_ns: 2000000000 },
    { id: 3, kind: 'Lifecycle', payload: { kind: 'Lifecycle', payload: { kind: 'AgentRunStateChanged', payload: {} } }, wall_clock_ns: 11000000000 },
  ] },
};
assert.equal(computeRun(spanEntry).duration_s, 10);

// run 2
assert.deepEqual(r2.run_js.errors, { js_exception: 0, host_rejected: 0, child_tool_error: 0, timeout: 1, limit_hit: 1, pre_run_error: 1 });
assert.deepEqual(r2.run_js.limit_hit_kinds, { tool_calls: 1 });
assert.deepEqual(r2.shell.interp_inline_by_lang, { node: 1, awk: 1 });
assert.equal(r2.shell.heredoc_cmds, 1);
assert.equal(r2.shell.chain_ops.max, 1);

// run 3 (run_js 無効)
assert.equal(r3.run_js.calls, 0);
assert.equal(r3.shell.commands, 0);
assert.equal(r3.run_js_enabled, false);

// 集計と表
const agg = aggregate([r1, r2, r3, r1]);
const g = agg.find((x) => x.model === 'gpt-sol');
assert.equal(g.runs, 2);
assert.equal(g.mean.requests, 6);
const md = toMarkdown([r1, r2, r3], agg);
assert.match(md, /## モデル × 課題（平均）/);
JSON.parse(JSON.stringify({ runs: [r1, r2, r3], aggregate: agg }));

// プログラム的構造（主指標）と build 連結数
assert.deepEqual(r1.shell.programmatic_by_kind, { script: 1, loop: 0, textproc: 1, sed_multi: 0 });
assert.equal(r1.shell.programmatic_cmds, 2);
assert.equal(r1.shell.build_chain_cmds, 0);
const buildEntry = (commands, extra = {}) => ({
  run: { run_id: 'run-b', usage_requests: [], context: {} },
  result: { status: 'ok', verify: 'pass', ...extra },
  events: { events: commands.map((command, i) => ({ id: i + 1, kind: 'Tool', wall_clock_ns: (i + 1) * 1e9, payload: { kind: 'Tool', payload: { kind: 'ToolStarted', payload: { call_id: `c${i}`, tool_name: 'shell', run_id: 'run-b', input: { command } } } } })) },
});
const rb = computeRun(buildEntry([
  'cd /x && cargo fmt -p a -- --check && cargo clippy -p a -- -D warnings && cargo test -p a',
  'cargo test -p a 2>&1 | tail -20',
  "git ls-files crates | xargs wc -l | sort -rn | head",
  "sed -i 's/a/b/' x.rs y.rs",
  'ls && pwd',
]));
assert.equal(rb.shell.commands, 5);
assert.equal(rb.shell.build_chain_cmds, 1);
assert.equal(rb.shell.build_steps_total, 4);
assert.equal(rb.shell.build_steps.max, 3);
assert.deepEqual(rb.shell.programmatic_by_kind, { script: 0, loop: 1, textproc: 0, sed_multi: 1 });
assert.equal(rb.shell.programmatic_cmds, 2);
assert.deepEqual(rb.shell.bands, { '1': 4, '2-3': 0, '4-7': 1, '8+': 0 }); // 軽いコマンドと cargo の build 系は除いて数える（0 は帯 1）

// status が ok でなくても verify=pass の run を区別する
assert.equal(outcomeOf('ok', 'pass'), 'ok');
assert.equal(outcomeOf('error', 'pass'), 'error+pass');
assert.equal(outcomeOf('error', 'fail'), 'error');
assert.equal(outcomeOf('timeout', 'pass'), 'timeout+pass');
assert.equal(outcomeOf(null, null), null);
const re = computeRun(buildEntry(['ls'], { status: 'error', verify: 'pass', error_reason: 'invalid SSE stream' }));
assert.equal(re.outcome, 'error+pass');
assert.ok(re.notes.some((n) => n.includes('verify=pass') && n.includes('invalid SSE stream')));
const reAgg = aggregate([re, computeRun(buildEntry(['ls']))]);
assert.equal(reAgg[0].error_but_pass, 1);
assert.match(toMarkdown([re], aggregate([re])), /error\+pass/);

// loadResults: 余計なディレクトリ・退避された試行・場所と合わない result.json を読まない
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'metrics-load-'));
const put = (rel, result) => {
  fs.mkdirSync(path.join(tmp, rel), { recursive: true });
  fs.writeFileSync(path.join(tmp, rel, 'result.json'), JSON.stringify(result));
};
put('gpt-sol/t1/1', { model: 'gpt-sol', task: 't1', rep: '1', status: 'ok', verify: 'pass' });
put('gpt-sol/t1/1.attempt1', { model: 'gpt-sol', task: 't1', rep: '1', status: 'error', verify: 'fail' });
put('claude-opus/t1/1', { model: 'claude-opus', task: 't1', rep: '1', status: 'ok', verify: 'pass' });
put('prelim-copy/gpt-sol/1', { model: 'gpt-sol', task: 't1', rep: '1' }); // 別の outroot を直下に置いた形: パスと result.json が合わない
put('other-model/t1/1', { model: 'gpt-sol', task: 't1', rep: '1' });
fs.mkdirSync(path.join(tmp, 'timeline'));
fs.writeFileSync(path.join(tmp, 'timeline', 'gpt-sol-t1.txt'), 'x');
fs.mkdirSync(path.join(tmp, '.preflight'));
fs.writeFileSync(path.join(tmp, '.preflight', 'gpt-sol.ok'), '');
const loaded = loadResults(tmp, undefined);
assert.deepEqual(loaded.map((e) => `${e.label.model}/${e.label.task}/${e.label.rep}`), ['claude-opus/t1/1', 'gpt-sol/t1/1']);
assert.equal(loaded.skipped.length, 2 + 1); // attempt 1 件 + 場所の合わないもの 1 件 + prelim-copy（task=gpt-sol/rep=1 の階層が合わない）
const withAttempts = loadResults(tmp, undefined, { includeAttempts: true });
assert.ok(withAttempts.some((e) => e.label.rep === '1.attempt1'));
fs.rmSync(tmp, { recursive: true });

console.log('metrics-test: ok');
