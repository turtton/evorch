// fixture-metrics.json を作る。`evorch inspect run` / `evorch inspect events --run` の出力形式
// （crates/storage/src/inspect.rs, crates/evorch/src/inspect.rs）に合わせた合成データ。
// イベントの payload は EventKind の隣接タグ形式 {kind:"Tool", payload:{kind:"ToolStarted", payload:{...}}}。
// 実行: node test/gen-fixture.mjs > test/fixture-metrics.json
let nextId = 1;
let ns = 1_800_000_000_000_000_000;
const tick = (ms = 500) => (ns += ms * 1_000_000);

function build(runId, steps) {
  const events = [];
  const push = (kind, body) => {
    events.push({
      id: nextId++,
      session_id: null,
      wall_clock_ns: tick(),
      kind: 'Tool',
      payload: { kind: 'Tool', payload: { kind, payload: { run_id: runId, ...body } } },
    });
  };
  for (const s of steps) {
    push('ToolStarted', { tool_name: s.tool, call_id: s.id, input: s.input, ...(s.parent ? { parent_call_id: s.parent } : {}) });
    push('ToolCompleted', { tool_name: s.tool, call_id: s.id, is_error: !!s.error, output: 'out', ...(s.detail ? { detail: s.detail } : {}), ...(s.parent ? { parent_call_id: s.parent } : {}) });
  }
  return { db: '/x/run.db', result: { scope: { run_id: runId }, truncated_older: false, events } };
}

const usage = (runId, i, purpose, extra = {}) => ({
  request_id: `${runId}-req-${i}`,
  at_ns: ns + i,
  provider: 'p',
  model: 'm',
  run_id: runId,
  role: 'worker',
  purpose,
  status: 'ok',
  input_tokens: 1000 * (i + 1),
  output_tokens: 100 + i,
  cache_read_tokens: 800 * i,
  cache_write_tokens: i === 0 ? 500 : 0,
  reasoning_tokens: 50,
  cost_usd: 0.01,
  ...extra,
});
const runDoc = (runId, rows) => ({ db: '/x/run.db', result: { run_id: runId, context: { role: 'Worker', terminal_phase: 'Done' }, children: [], usage_requests: rows, event_kinds: {} } });
const rjDetail = (calls, extra = {}) => ({ run_js: { calls, calls_omitted: 0, output_bytes: 120, return_bytes: 40, timed_out: false, limit_hit: null, written_paths: [], ...extra } });
const call = (seq, name, is_error = false) => ({ seq, name, is_error, paths: [] });

// run 1: run_js を使う調査課題
const r1 = 'run-fixture-1';
const run1 = {
  label: { model: 'gpt-sol', task: 't1-survey', rep: '1' },
  result: { model: 'gpt-sol', task: 't1-survey', rep: '1', run_js_enabled: true, run_id: r1, status: 'ok', verify: 'pass', duration_ms: 95000 },
  run: runDoc(r1, [0, 1, 2, 3, 4, 5].map((i) => usage(r1, i, i === 0 ? null : 'agent')).concat([usage(r1, 9, 'title', { cost_usd: 0.001, input_tokens: 50 })])),
  events: build(r1, [
    { tool: 'read', id: 'c1', input: { path: 'a.rs' } },
    { tool: 'run_js', id: 'c2', input: { code: 'const f = await tools.grep({pattern: "x"}); return f;' }, detail: rjDetail([call(1, 'grep'), call(2, 'read'), call(3, 'read')], { output_bytes: 300, return_bytes: 2000 }) },
    { tool: 'grep', id: 'c2/1', parent: 'c2', input: {} },
    { tool: 'read', id: 'c2/2', parent: 'c2', input: {} },
    { tool: 'read', id: 'c2/3', parent: 'c2', input: {} },
    // JS 例外
    { tool: 'run_js', id: 'c3', input: { code: 'throw new Error("x")' }, error: true, detail: rjDetail([]) },
    // host 拒否 1 件 + 子ツールのエラー 1 件
    { tool: 'run_js', id: 'c4', input: { code: 'await tools.edit({}); await tools.shell({command:"cargo test"})' }, detail: rjDetail([call(1, 'edit', true), call(2, 'shell', true), call(3, 'read')]) },
    { tool: 'shell', id: 'c4/2', parent: 'c4', input: { command: 'cargo test' }, error: true },
    { tool: 'read', id: 'c4/3', parent: 'c4', input: {} },
    { tool: 'shell', id: 'c5', input: { command: "python3 - <<'PY'\nimport json\nprint(1 | 2)\nPY" } },
    { tool: 'shell', id: 'c6', input: { command: "grep -rn foo crates | sort | uniq -c | head -n 20" } },
    { tool: 'shell', id: 'c7', input: { action: 'poll', job_id: 'job-1' } },
  ]),
};

// run 2: shell 中心。timeout と limit_hit
const r2 = 'run-fixture-2';
const run2 = {
  label: { model: 'claude-opus', task: 't3-rename', rep: '1' },
  result: { model: 'claude-opus', task: 't3-rename', rep: '1', run_js_enabled: true, run_id: r2, status: 'ok', verify: 'fail', duration_ms: 180000 },
  run: runDoc(r2, [0, 1, 2, 3].map((i) => usage(r2, i, 'agent'))),
  events: build(r2, [
    { tool: 'shell', id: 'd1', input: { command: "grep -rln OldName crates/apply-patch | xargs sed -i 's/OldName/NewName/g' && cargo test -p apply-patch 2>&1 | tail -n 30" } },
    { tool: 'shell', id: 'd2', input: { command: "node -e 'console.log(require(\"fs\").readdirSync(\".\").length)'" } },
    { tool: 'shell', id: 'd3', input: { command: "ls | awk '{print $1}'; cat x <<EOF\nhi\nEOF" } },
    { tool: 'run_js', id: 'd4', input: { code: 'while(true){}', timeout_ms: 1000 }, error: true, detail: rjDetail([], { timed_out: true, limit_hit: 'timeout' }) },
    { tool: 'run_js', id: 'd5', input: { code: 'for(;;) await tools.read({path:"a"})' }, error: true, detail: rjDetail([call(1, 'read')], { limit_hit: 'tool_calls' }) },
    { tool: 'run_js', id: 'd6', input: { timeout_ms: 0 }, error: true },
    { tool: 'read', id: 'd5/1', parent: 'd5', input: {} },
  ]),
};

// run 3: run_js 無効の対照
const r3 = 'run-fixture-3';
const run3 = {
  label: { model: 'gpt-astra', task: 't5-single-edit', rep: '1' },
  result: { model: 'gpt-astra', task: 't5-single-edit', rep: '1', run_js_enabled: false, run_id: r3, status: 'ok', verify: 'pass', duration_ms: 20000 },
  run: runDoc(r3, [usage(r3, 0, 'agent'), usage(r3, 1, 'agent')]),
  events: build(r3, [
    { tool: 'grep', id: 'e1', input: {} },
    { tool: 'edit', id: 'e2', input: {} },
  ]),
};

console.log(JSON.stringify({ description: 'synthetic inspect-format fixture for metrics.mjs', runs: [run1, run2, run3] }, null, 1));
