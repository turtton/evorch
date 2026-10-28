// metrics.mjs の確認: fixture-metrics.json の期待値。実行: node test/metrics-test.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import { aggregate, computeRun, toMarkdown } from '../scripts/metrics.mjs';

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
console.log('metrics-test: ok');
