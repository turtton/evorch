#!/usr/bin/env node
// run ごとの指標を集計する（ADR 0039 第 1 段階の観察用）。bun / node で動く。npm 依存なし。
//
// 使い方:
//   metrics.mjs --results <outroot> [--json out.json] [--md out.md]
//       run-one.sh / run-matrix.sh の結果ディレクトリを読む（各 run の result.json と、保存済みの
//       inspect-run.json / inspect-events.json。無ければ run.db を `evorch inspect` で読む）。
//   metrics.mjs --db <path> --run <run-id>[=<model>/<task>/<rep>] [--run ...]
//       DB と run_id を直接指定する。
//   metrics.mjs --fixture <file.json>
//       `evorch inspect` の出力形式の fixture から読む（テスト用。形式は test/fixture-metrics.json）。
// 共通:
//   --evorch <bin>       `evorch inspect` に使うバイナリ（既定 $EVORCH_BIN）
//   --format md|json|both   標準出力に出すもの（既定 md）。--json / --md はファイルへ書く。
//   --include-attempts   --retry-errors が退避した <rep>.attempt<N> も読む（既定は読まない）
//
// 指標の定義は README.md の「指標」を参照。

import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

// ---------------------------------------------------------------- shell 解析

/**
 * ヒアドキュメント本体・引用符の中身・コメントを取り除いた文字列と、ヒアドキュメント数を返す。
 * `keepQuotes` なら引用符の中身は残す（パスの取り出し用。本体とコメントは取り除く）。
 */
export function sanitizeShell(source, { keepQuotes = false } = {}) {
  const s = String(source);
  const n = s.length;
  let out = '';
  let i = 0;
  let heredocs = 0;
  let hereStrings = 0;
  let wordStart = true;
  const pending = [];
  while (i < n) {
    const c = s[i];
    if (c === '\\') {
      if (s[i + 1] === '\n') {
        out += ' ';
      } else {
        out += 'x';
        wordStart = false;
      }
      i += 2;
      continue;
    }
    if (c === "'") {
      let j = s.indexOf("'", i + 1);
      if (j < 0) j = n - 1;
      out += keepQuotes ? s.slice(i, j + 1) : 'Q';
      i = j + 1;
      wordStart = false;
      continue;
    }
    if (c === '"') {
      let j = i + 1;
      while (j < n && s[j] !== '"') j += s[j] === '\\' ? 2 : 1;
      out += keepQuotes ? s.slice(i, j + 1) : 'Q';
      i = j + 1;
      wordStart = false;
      continue;
    }
    if (c === '#' && wordStart) {
      while (i < n && s[i] !== '\n') i++;
      continue;
    }
    if (c === '\n') {
      out += '\n';
      i++;
      while (pending.length > 0) {
        const { delim, strip } = pending.shift();
        while (i < n) {
          let e = s.indexOf('\n', i);
          if (e < 0) e = n;
          const line = s.slice(i, e);
          i = Math.min(e + 1, n);
          if ((strip ? line.replace(/^\t+/, '') : line) === delim) break;
        }
      }
      wordStart = true;
      continue;
    }
    if (c === '<' && s[i + 1] === '<') {
      if (s[i + 2] === '<') {
        hereStrings++;
        out += '<<<';
        i += 3;
        wordStart = true;
        continue;
      }
      let j = i + 2;
      let strip = false;
      if (s[j] === '-') {
        strip = true;
        j++;
      }
      while (s[j] === ' ' || s[j] === '\t') j++;
      let delim = '';
      while (j < n && !/[\s;&|<>()]/.test(s[j])) {
        if (s[j] === "'" || s[j] === '"') {
          const q = s[j];
          const e = s.indexOf(q, j + 1);
          const end = e < 0 ? n : e;
          delim += s.slice(j + 1, end);
          j = end + 1;
        } else if (s[j] === '\\') {
          delim += s[j + 1] ?? '';
          j += 2;
        } else {
          delim += s[j];
          j++;
        }
      }
      if (delim !== '') {
        heredocs++;
        pending.push({ delim, strip });
      }
      out += '<<H';
      i = j;
      wordStart = false;
      continue;
    }
    out += c;
    wordStart = /[\s;&|(]/.test(c);
    i++;
  }
  return { text: out, heredocs, hereStrings };
}

const WRAPPERS = new Set(['then', 'do', 'else', 'elif', 'if', 'while', 'until', 'time', 'exec', 'command', 'nohup', 'sudo', 'env', 'xargs', 'builtin']);
const CLOSERS = new Set(['done', 'fi', 'esac', '}', ')']);
const FLAG_WITH_VALUE = new Set(['-I', '-n', '-P', '-L', '-d', '-s', '-u']);

/** 単純コマンド 1 つを分解する: 先頭の環境変数代入・制御語・ラッパー（xargs など）を読み飛ばした実コマンド。 */
function commandOf(segment) {
  const hasHere = /<<H|<<</.test(segment);
  const tokens = segment.trim().split(/\s+/).filter(Boolean);
  const out = { program: null, args: [], hasHere, loopKeyword: false, viaXargs: false, bare: true };
  if (tokens.length === 0) return out;
  tokens[0] = tokens[0].replace(/^[({!`$]+/, '');
  if (/^(for|while|until)$/.test(tokens[0])) out.loopKeyword = true;
  if (CLOSERS.has(tokens[0])) return out;
  let k = 0;
  let wrapped = false;
  while (k < tokens.length) {
    const t = tokens[k];
    if (t === '') k++;
    else if (/^\w+=/.test(t)) k++;
    else if (WRAPPERS.has(t)) {
      wrapped = true;
      if (t === 'xargs') out.viaXargs = true;
      k++;
    } else if (wrapped && t.startsWith('-')) {
      k += FLAG_WITH_VALUE.has(t) ? 2 : 1;
    } else break;
  }
  if (k >= tokens.length) return out;
  out.bare = false;
  out.program = tokens[k].split('/').pop();
  out.args = tokens.slice(k + 1);
  return out;
}

/** 1 セグメント（単純コマンド）から、インタプリタ実行を判定する。 */
function interpreterOf(cmd) {
  const { program, args, hasHere } = cmd;
  if (program == null) return null;
  const has = (re) => args.some((a) => re.test(a));
  if (/^(node|nodejs|bun|deno)$/.test(program)) {
    if (has(/^(-e|-p|-pe|--eval|--print|-)$/) || hasHere) return { lang: 'node', mode: 'inline' };
    if (has(/\.[cm]?[jt]sx?$/)) return { lang: 'node', mode: 'file' };
    return null;
  }
  if (/^(python|python3|python3\.\d+|pypy3?)$/.test(program)) {
    if (has(/^(-c|-)$/) || hasHere) return { lang: 'python', mode: 'inline' };
    if (has(/\.py$/)) return { lang: 'python', mode: 'file' };
    return null;
  }
  if (program === 'perl') {
    if (has(/^-[A-Za-z]*[eE]/) || hasHere) return { lang: 'perl', mode: 'inline' };
    if (has(/\.pl$/)) return { lang: 'perl', mode: 'file' };
    return null;
  }
  if (program === 'ruby') {
    if (has(/^-[A-Za-z]*e/) || hasHere) return { lang: 'ruby', mode: 'inline' };
    if (has(/\.rb$/)) return { lang: 'ruby', mode: 'file' };
    return null;
  }
  if (/^(awk|gawk|mawk|nawk)$/.test(program)) {
    return { lang: 'awk', mode: has(/^-f/) ? 'file' : 'inline' };
  }
  if (program === 'jq') {
    return { lang: 'jq', mode: has(/^(-f|--from-file)$/) ? 'file' : 'inline' };
  }
  if (/^(sh|bash|zsh|dash)$/.test(program) && hasHere) return { lang: 'sh', mode: 'inline' };
  return null;
}

const PATH = String.raw`(?:"([^"]+)"|'([^']+)'|([^\s"'<>|;&()]+))`;
const pick = (m) => m[1] ?? m[2] ?? m[3];

/** ヒアドキュメントで書き込むファイルのパス（`cat > f <<EOF`、`cat <<EOF > f`、`tee f <<EOF`）を、出てくる順に返す。 */
function heredocWritePaths(kept) {
  const found = [];
  const res = [
    new RegExp(String.raw`\bcat\b[^\n|;&<>]*>>?\s*${PATH}[^\n|;&<]*<<H`, 'g'),
    new RegExp(String.raw`\bcat\b\s*<<H\s*>>?\s*${PATH}`, 'g'),
    new RegExp(String.raw`\btee\b\s+(?:-[a-z]+\s+)*${PATH}[^\n|;&<]*<<H`, 'g'),
  ];
  for (const re of res) for (const m of kept.matchAll(re)) found.push({ at: m.index, path: pick(m) });
  return found.sort((a, b) => a.at - b.at).map((f) => f.path);
}

/** コマンドが実行するスクリプトファイルのパス（`python3 x.py`、`node x.mjs`、`bash x.sh`、`./x.sh`、`awk -f x.awk`、`source x.sh`）。 */
function fileRunPaths(kept) {
  const found = [];
  const interp = String.raw`\b(?:python3?(?:\.\d+)?|pypy3?|node|nodejs|bun|deno|perl|ruby|bash|sh|zsh|dash)\s+(?:-(?![ceEp]\b|-eval)[A-Za-z-]+\s+)*(?!-)`;
  for (const m of kept.matchAll(new RegExp(interp + PATH, 'g'))) found.push(pick(m));
  for (const m of kept.matchAll(new RegExp(String.raw`\b(?:awk|gawk|mawk)\s+(?:-[A-Za-z-]+\s+)*-f\s*${PATH}`, 'g'))) found.push(pick(m));
  for (const m of kept.matchAll(new RegExp(String.raw`(?:^|[;&|\n(])\s*(\.?\.?/[^\s"'<>|;&()]+)`, 'gm'))) found.push(m[1]);
  for (const m of kept.matchAll(new RegExp(String.raw`\b(?:source|\.)\s+${PATH}`, 'g'))) found.push(pick(m));
  return found.filter(Boolean);
}

/** `segments` の数から除く、ナビゲーション・確認だけの軽いコマンド（cd / pwd / ls / echo / git status / which）。 */
function isTrivial(cmd) {
  if (cmd.program == null) return false;
  if (/^(cd|pwd|ls|echo|which)$/.test(cmd.program)) return true;
  if (cmd.program === 'git') {
    for (let k = 0; k < cmd.args.length; k++) {
      const a = cmd.args[k];
      if (a === '-C' || a === '-c') k++;
      else if (!a.startsWith('-')) return a === 'status';
    }
  }
  return false;
}

const BUILD_SUBCOMMANDS = new Set(['fmt', 'clippy', 'test', 'check', 'build', 'nextest']);

/** `cargo fmt|clippy|test|check|build|nextest` の呼び出しか。`+toolchain` やフラグは読み飛ばす。 */
function isBuildStep(cmd) {
  if (cmd.program !== 'cargo') return false;
  const sub = cmd.args.find((a) => !a.startsWith('-') && !a.startsWith('+'));
  return sub != null && BUILD_SUBCOMMANDS.has(sub);
}

const TEXT_STAGE = /^(grep|egrep|fgrep|rg|sed|awk|gawk|mawk|sort|uniq|cut|tr|wc)$/;

/** `sed -i`（インプレース編集）なら、編集対象ファイルの数の見積り（glob は 2 と数える）。違えば null。 */
function sedInPlaceFiles(cmd) {
  if (cmd.program !== 'sed') return null;
  if (!cmd.args.some((a) => /^-[A-Za-z]*i/.test(a) || /^--in-place/.test(a))) return null;
  const operands = cmd.args.filter((a) => !a.startsWith('-'));
  const scriptGiven = cmd.args.some((a) => /^(-e|-f|--expression|--file)/.test(a));
  const files = scriptGiven ? operands : operands.slice(1);
  return files.reduce((n, f) => n + (f.includes('*') ? 2 : 1), 0);
}

/** 1 つの shell コマンド文字列の構造指標。 */
export function analyzeShell(command) {
  const raw = String(command);
  const { text, heredocs, hereStrings } = sanitizeShell(raw);
  let pipes = 0;
  let chain = 0;
  const segments = [];
  let cur = '';
  const push = (terminator) => {
    segments.push({ text: cur, terminator });
    cur = '';
  };
  for (let k = 0; k < text.length; k++) {
    const c = text[k];
    const next = text[k + 1];
    const prev = text[k - 1];
    if (c === '&' && next === '&') {
      chain++;
      push('&&');
      k++;
    } else if (c === '|' && next === '|') {
      chain++;
      push('||');
      k++;
    } else if (c === '|' && prev === '>') {
      cur += c; // `>|`
    } else if (c === '|') {
      pipes++;
      if (next === '&') k++;
      push('|');
    } else if (c === ';') {
      chain++;
      if (next === ';') k++;
      push(';');
    } else if (c === '&') {
      if (prev === '>' || prev === '<' || next === '>') cur += c; // リダイレクト
      else {
        chain++;
        push('&');
      }
    } else if (c === '\n') {
      push('\n');
    } else {
      cur += c;
    }
  }
  push('');
  const nonEmpty = (seg) => seg.text.trim() !== '';
  const realSegs = segments.filter(nonEmpty);
  let newlineSeps = 0;
  segments.forEach((seg, idx) => {
    if (seg.terminator === '\n' && nonEmpty(seg) && segments.slice(idx + 1).some(nonEmpty)) newlineSeps++;
  });
  const cmds = realSegs.map((seg) => ({ seg, cmd: commandOf(seg.text) }));
  const interps = cmds.map(({ cmd }) => interpreterOf(cmd)).filter(Boolean);

  // --- build 連結（cargo fmt / clippy / test / check ...）と、軽いコマンド・制御語だけの区切りは segments から除く
  const buildSteps = cmds.filter(({ cmd }) => isBuildStep(cmd)).length;
  const trivial = cmds.filter(({ cmd }) => isTrivial(cmd)).length;
  const bare = cmds.filter(({ cmd }) => cmd.bare && !cmd.loopKeyword).length;
  const countedSegments = cmds.filter(({ cmd }) => !isBuildStep(cmd) && !isTrivial(cmd) && !(cmd.bare && !cmd.loopKeyword)).length;

  // --- プログラム的構造（4 種）
  const inlineInterp = interps.some((i) => i.mode === 'inline' && i.lang !== 'jq');
  // ヒアドキュメントの受け手で分ける: インタプリタに食わせる（script）／ファイルへ書くだけ（write）／それ以外（other。`git commit -F -` など）。
  // `write` は、同じ run の後のコマンドがそのファイルを実行したときだけ script に数える（linkWrittenScripts）。
  const kept = sanitizeShell(raw, { keepQuotes: true }).text;
  const writeTargets = heredocWritePaths(kept);
  let writeIndex = 0;
  const heredocKinds = [];
  for (const { seg, cmd } of cmds) {
    if (!/<<H/.test(seg.text)) continue;
    const ip = interpreterOf(cmd);
    if (ip && ip.mode === 'inline' && ip.lang !== 'jq') heredocKinds.push({ kind: 'interp' });
    else if (cmd.program === 'cat' || cmd.program === 'tee') heredocKinds.push({ kind: 'write', path: writeTargets[writeIndex++] ?? null });
    else heredocKinds.push({ kind: 'other' });
  }
  const script = inlineInterp;
  const loop = cmds.some(({ cmd }) => cmd.loopKeyword || cmd.program === 'xargs' || cmd.viaXargs);
  // 連続するパイプライン（`|` でつながる段）ごとに、テキスト処理の段数を数える。head / tail は段に数えない。
  let textproc = false;
  let stages = 0;
  cmds.forEach(({ cmd }, idx) => {
    const prevLinked = idx > 0 && cmds[idx - 1].seg.terminator === '|';
    if (!prevLinked) stages = 0;
    if (cmd.program != null && TEXT_STAGE.test(cmd.program)) stages++;
    if (stages >= 3) textproc = true;
  });
  const sedFiles = cmds.map(({ cmd }) => sedInPlaceFiles(cmd)).filter((n) => n != null);
  const sedMulti = sedFiles.length > 0 && (sedFiles.reduce((a, b) => a + b, 0) >= 2 || loop);
  return {
    chars: raw.length,
    lines: raw.split('\n').length,
    pipes,
    chain_ops: chain,
    newline_seps: newlineSeps,
    segments: countedSegments,
    segments_raw: realSegs.length,
    segments_excluded: { trivial, build: buildSteps, control: bare },
    build_steps: buildSteps,
    heredocs,
    here_strings: hereStrings,
    interpreters: interps,
    heredoc_kinds: heredocKinds,
    runs: fileRunPaths(kept),
    programmatic: { script, loop, textproc, sed_multi: sedMulti },
  };
}

const basename = (p) => String(p).split('/').pop();

/**
 * 1 つの run のトップレベル shell コマンド（実行順の analyzeShell の結果）を照合する。`cat > x.py <<EOF` のように
 * ファイルへ書いただけのヒアドキュメントは、同じコマンドか後のコマンドがそのファイル（ファイル名で照合）を実行したときだけ
 * script に数える（`write_run`）。実行されなければ `write_only`。コマンドを書き換えて、種類別の件数を返す。
 */
export function linkWrittenScripts(commands) {
  const counts = { interp: 0, write_run: 0, write_only: 0, other: 0 };
  commands.forEach((c, i) => {
    for (const h of c.heredoc_kinds) {
      if (h.kind === 'write') {
        const target = h.path ? basename(h.path) : null;
        const ran = target != null && commands.slice(i).some((d) => d.runs.some((r) => basename(r) === target));
        h.kind = ran ? 'write_run' : 'write_only';
        if (ran) c.programmatic.script = true;
      }
      counts[h.kind]++;
    }
  });
  return counts;
}

// ---------------------------------------------------------------- 統計

function quantile(sorted, q) {
  if (sorted.length === 0) return null;
  const pos = (sorted.length - 1) * q;
  const lo = Math.floor(pos);
  const hi = Math.ceil(pos);
  return sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo);
}

export function summarize(values) {
  const v = values.filter((x) => typeof x === 'number' && Number.isFinite(x)).sort((a, b) => a - b);
  if (v.length === 0) return { n: 0, mean: null, median: null, p90: null, max: null };
  const sum = v.reduce((a, b) => a + b, 0);
  return { n: v.length, mean: sum / v.length, median: quantile(v, 0.5), p90: quantile(v, 0.9), max: v[v.length - 1] };
}

const bandOf = (segments) => (segments <= 1 ? '1' : segments <= 3 ? '2-3' : segments <= 7 ? '4-7' : '8+');

// ---------------------------------------------------------------- イベント

/** `evorch inspect` の出力から結果本体を取り出す（`{db, result}` 形式でも素の形式でも可）。 */
const unwrap = (doc) => (doc && typeof doc === 'object' && 'result' in doc && 'db' in doc ? doc.result : doc);

/**
 * 保存されたイベントから ToolStarted / ToolCompleted の本体を取り出す。
 * payload は EventKind の隣接タグ形式 {kind:"Tool", payload:{kind:"ToolStarted", payload:{...}}} だが、
 * 内側だけ・平らな形も受け付ける。
 */
export function toolEvents(events) {
  const out = [];
  for (const event of events ?? []) {
    let cur = event?.payload;
    for (let depth = 0; depth < 4 && cur && typeof cur === 'object'; depth++) {
      if (cur.kind === 'ToolStarted' || cur.kind === 'ToolCompleted') {
        if (cur.payload && typeof cur.payload === 'object') out.push({ type: cur.kind, id: event.id, ns: event.wall_clock_ns, ...cur.payload });
        cur = null;
        break;
      }
      if (cur.payload !== undefined && 'kind' in cur) cur = cur.payload;
      else break;
    }
  }
  return out;
}

// ---------------------------------------------------------------- 1 run の集計

const isAgentRequest = (row) => row.purpose == null || row.purpose === '' || row.purpose === 'agent';
const num = (x) => (typeof x === 'number' && Number.isFinite(x) ? x : 0);
const add = (map, key, by = 1) => {
  map[key] = (map[key] ?? 0) + by;
};

/** status と verify から実行結果の区分を決める。status が ok でなくても verify=pass の run は `<status>+pass` と区別する。 */
export function outcomeOf(status, verify) {
  if (status == null) return verify == null ? null : `verify-${verify}`;
  if (status === 'ok') return 'ok';
  return verify === 'pass' ? `${status}+pass` : status;
}

export function computeRun(entry) {
  const run = unwrap(entry.run) ?? {};
  const eventsDoc = unwrap(entry.events) ?? {};
  const runId = run.run_id ?? entry.result?.run_id ?? entry.runId ?? null;
  const notes = [];

  // --- リクエスト（usage_requests）
  const rows = run.usage_requests ?? [];
  const agentRows = rows.filter(isAgentRequest);
  const auxRows = rows.filter((r) => !isAgentRequest(r));
  const sum = (list, key) => list.reduce((a, r) => a + num(r[key]), 0);
  const unpriced = rows.filter((r) => r.cost_usd == null).length;
  if (unpriced > 0) notes.push(`${unpriced} 件のリクエストに費用がない（未知の価格）`);
  const usage = {
    requests: agentRows.length,
    requests_failed: agentRows.filter((r) => r.status === 'failed').length,
    aux_requests: auxRows.length,
    aux_purposes: auxRows.reduce((m, r) => (add(m, r.purpose), m), {}),
    input_tokens: sum(agentRows, 'input_tokens'),
    cache_read_tokens: sum(agentRows, 'cache_read_tokens'),
    cache_write_tokens: sum(agentRows, 'cache_write_tokens'),
    output_tokens: sum(agentRows, 'output_tokens'),
    reasoning_tokens: sum(agentRows, 'reasoning_tokens'),
    cost_usd: sum(agentRows, 'cost_usd'),
    aux_cost_usd: sum(auxRows, 'cost_usd'),
    unpriced_requests: unpriced,
  };
  usage.total_cost_usd = usage.cost_usd + usage.aux_cost_usd;

  // --- ツール
  const events = toolEvents(eventsDoc.events);
  if (eventsDoc.truncated_older) notes.push('events が上限で切れている（古いイベントが欠落）');
  if (!eventsDoc.events) notes.push('イベントが読めていない');
  const own = events.filter((e) => e.run_id == null || runId == null || e.run_id === runId);
  const started = new Map();
  const completed = new Map();
  for (const e of own) {
    if (e.type === 'ToolStarted' && !started.has(e.call_id)) started.set(e.call_id, e);
    if (e.type === 'ToolCompleted') completed.set(e.call_id, e);
  }
  const top = [...started.values()].filter((e) => !e.parent_call_id);
  const children = [...started.values()].filter((e) => e.parent_call_id);
  const toolsByName = {};
  let topErrors = 0;
  for (const e of top) {
    add(toolsByName, e.tool_name);
    if (completed.get(e.call_id)?.is_error) topErrors++;
  }
  const childByTool = {};
  for (const e of children) add(childByTool, e.tool_name);
  const childErrors = [...completed.values()].filter((e) => e.parent_call_id && e.is_error);

  // --- run_js
  const rjStarted = top.filter((e) => e.tool_name === 'run_js');
  const rj = {
    calls: rjStarted.length,
    code_chars: 0,
    output_bytes: 0,
    return_bytes: 0,
    child_calls: 0,
    child_calls_omitted: 0,
    errors: { js_exception: 0, host_rejected: 0, child_tool_error: 0, timeout: 0, limit_hit: 0, pre_run_error: 0 },
    limit_hit_kinds: {},
    written_paths: 0,
  };
  for (const start of rjStarted) {
    rj.code_chars += typeof start.input?.code === 'string' ? start.input.code.length : 0;
    const done = completed.get(start.call_id);
    const detail = done?.detail?.run_js;
    if (!detail) {
      if (done?.is_error) rj.errors.pre_run_error++;
      continue;
    }
    const calls = detail.calls ?? [];
    rj.child_calls += calls.length + num(detail.calls_omitted);
    rj.child_calls_omitted += num(detail.calls_omitted);
    rj.output_bytes += num(detail.output_bytes);
    rj.return_bytes += num(detail.return_bytes);
    rj.written_paths += (detail.written_paths ?? []).length;
    const prefix = `${start.call_id}/`;
    const failedCalls = calls.filter((c) => c.is_error).length;
    const childErr = childErrors.filter((e) => e.call_id.startsWith(prefix)).length;
    rj.errors.child_tool_error += Math.min(failedCalls, childErr);
    rj.errors.host_rejected += Math.max(0, failedCalls - childErr);
    if (detail.timed_out) rj.errors.timeout++;
    if (detail.limit_hit && detail.limit_hit !== 'timeout') {
      rj.errors.limit_hit++;
      add(rj.limit_hit_kinds, detail.limit_hit);
    }
    if (done.is_error && !detail.timed_out && !detail.limit_hit) rj.errors.js_exception++;
  }
  rj.error_runs = rj.errors.js_exception + rj.errors.timeout + rj.errors.limit_hit + rj.errors.pre_run_error;

  // --- shell
  const shellStarts = top.filter((e) => e.tool_name === 'shell');
  const commands = [];
  let shellControl = 0;
  for (const e of shellStarts) {
    if (typeof e.input?.command === 'string' && (e.input.action == null || e.input.action === 'start')) commands.push(analyzeShell(e.input.command));
    else shellControl++;
  }
  const heredocByKind = linkWrittenScripts(commands);
  const langs = {};
  let interpFile = 0;
  for (const c of commands) {
    for (const i of c.interpreters) {
      if (i.mode === 'inline') add(langs, i.lang);
      else interpFile++;
    }
  }
  const inlineCmds = commands.filter((c) => c.interpreters.some((i) => i.mode === 'inline' && i.lang !== 'jq'));
  const bands = { '1': 0, '2-3': 0, '4-7': 0, '8+': 0 };
  for (const c of commands) bands[bandOf(c.segments)]++;
  const progKinds = { script: 0, loop: 0, textproc: 0, sed_multi: 0 };
  for (const c of commands) for (const k of Object.keys(progKinds)) if (c.programmatic[k]) progKinds[k]++;
  const programmatic = commands.filter((c) => Object.values(c.programmatic).some(Boolean));
  const shell = {
    commands: commands.length,
    control_calls: shellControl,
    child_commands: childByTool.shell ?? 0,
    programmatic_cmds: programmatic.length,
    programmatic_by_kind: progKinds,
    build_chain_cmds: commands.filter((c) => c.build_steps >= 2).length,
    build_steps_total: commands.reduce((a, c) => a + c.build_steps, 0),
    build_steps: summarize(commands.map((c) => c.build_steps)),
    heredoc_cmds: commands.filter((c) => c.heredocs > 0).length,
    heredoc_by_kind: heredocByKind,
    heredoc_write_only_cmds: commands.filter((c) => c.heredoc_kinds.length > 0 && c.heredoc_kinds.every((h) => h.kind === 'write_only')).length,
    here_string_cmds: commands.filter((c) => c.here_strings > 0).length,
    interp_inline_cmds: inlineCmds.length,
    interp_inline_by_lang: langs,
    interp_file_runs: interpFile,
    oneliner_cmds: commands.filter((c) => c.lines === 1).length,
    bands,
    chars: summarize(commands.map((c) => c.chars)),
    lines: summarize(commands.map((c) => c.lines)),
    pipes: summarize(commands.map((c) => c.pipes)),
    chain_ops: summarize(commands.map((c) => c.chain_ops)),
    segments: summarize(commands.map((c) => c.segments)),
  };

  // --- 時間・状態
  // 所要時間はツール以外も含む全イベントの最初〜最後（ツールイベントだけだと実時間より短くなる）。
  const stamps = (eventsDoc.events ?? []).map((e) => e?.wall_clock_ns).filter((x) => typeof x === 'number');
  const eventSpanMs = stamps.length > 1 ? (Math.max(...stamps) - Math.min(...stamps)) / 1e6 : null;
  const result = entry.result ?? {};
  const label = entry.label ?? {};
  const outcome = outcomeOf(result.status, result.verify);
  if (result.status === 'aborted') notes.push(`aborted（${result.abort_kind ?? '?'}）。費用・リクエスト数は打ち切るまでの値で、他の run と平均しない`);
  if (outcome?.endsWith('+pass')) notes.push(`${result.status} だが verify=pass（成果物は正しい）${result.error_reason ? `: ${result.error_reason}` : ''}`);
  else if (result.status && result.status !== 'ok' && result.error_reason) notes.push(`${result.status}: ${result.error_reason}`);

  return {
    model: label.model ?? result.model ?? null,
    task: label.task ?? result.task ?? null,
    rep: label.rep ?? result.rep ?? null,
    run_js_enabled: result.run_js_enabled ?? null,
    run_id: runId,
    status: result.status ?? null,
    outcome,
    error_reason: result.error_reason ?? null,
    abort_kind: result.abort_kind ?? null,
    verify: result.verify ?? null,
    duration_s: (result.duration_ms ?? eventSpanMs) == null ? null : (result.duration_ms ?? eventSpanMs) / 1000,
    terminal_phase: run.context?.terminal_phase ?? null,
    usage,
    tools: {
      top_level_calls: top.length,
      top_level_errors: topErrors,
      by_name: toolsByName,
      child_executed: children.length,
      child_by_tool: childByTool,
    },
    run_js: rj,
    shell,
    notes,
  };
}

// ---------------------------------------------------------------- 集計表

/** 表と平均に使う平らな数値列。 */
function flat(r) {
  return {
    requests: r.usage.requests,
    aux_requests: r.usage.aux_requests,
    input: r.usage.input_tokens,
    cache_read: r.usage.cache_read_tokens,
    cache_write: r.usage.cache_write_tokens,
    output: r.usage.output_tokens,
    reasoning: r.usage.reasoning_tokens,
    cost_usd: r.usage.cost_usd,
    total_cost_usd: r.usage.total_cost_usd,
    duration_s: r.duration_s,
    tool_calls: r.tools.top_level_calls,
    run_js_calls: r.run_js.calls,
    child_calls: r.run_js.child_calls,
    run_js_js_exception: r.run_js.errors.js_exception,
    run_js_host_rejected: r.run_js.errors.host_rejected,
    run_js_timeout: r.run_js.errors.timeout,
    run_js_limit_hit: r.run_js.errors.limit_hit,
    run_js_output_bytes: r.run_js.output_bytes,
    run_js_return_bytes: r.run_js.return_bytes,
    shell_cmds: r.shell.commands,
    programmatic_cmds: r.shell.programmatic_cmds,
    build_chain_cmds: r.shell.build_chain_cmds,
    heredoc_cmds: r.shell.heredoc_cmds,
    interp_cmds: r.shell.interp_inline_cmds,
    pipes_max: r.shell.pipes.max,
    chain_max: r.shell.chain_ops.max,
    chars_median: r.shell.chars.median,
  };
}

export function aggregate(runs) {
  const groups = new Map();
  for (const r of runs) {
    const key = [r.model, r.task, r.run_js_enabled].join('\u0000');
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(r);
  }
  const out = [];
  for (const list of groups.values()) {
    const means = {};
    for (const k of Object.keys(flat(list[0]))) {
      const vals = list.map((r) => flat(r)[k]).filter((x) => typeof x === 'number');
      means[k] = vals.length ? vals.reduce((a, b) => a + b, 0) / vals.length : null;
    }
    out.push({
      model: list[0].model,
      task: list[0].task,
      run_js_enabled: list[0].run_js_enabled,
      runs: list.length,
      verify_pass: list.filter((r) => r.verify === 'pass').length,
      verify_known: list.filter((r) => r.verify != null).length,
      error_but_pass: list.filter((r) => String(r.outcome ?? '').endsWith('+pass')).length,
      failed_status: list.filter((r) => r.status != null && r.status !== 'ok').length,
      aborted: list.filter((r) => r.status === 'aborted').length,
      mean: means,
    });
  }
  out.sort((a, b) => String(a.task).localeCompare(String(b.task)) || String(a.model).localeCompare(String(b.model)) || String(a.run_js_enabled).localeCompare(String(b.run_js_enabled)));
  return out;
}

const fmt = (x, digits = 0) => (x == null ? '-' : Number.isInteger(x) && digits === 0 ? String(x) : x.toFixed(digits));
const compact = (map) => Object.entries(map).map(([k, v]) => `${k}=${v}`).join(',') || '-';

export function toMarkdown(runs, agg) {
  const lines = [];
  const table = (head, body) => {
    lines.push(`| ${head.join(' | ')} |`, `|${head.map(() => '---').join('|')}|`);
    for (const row of body) lines.push(`| ${row.join(' | ')} |`);
    lines.push('');
  };
  lines.push('## run ごと', '');
  table(
    ['model', 'task', 'rep', 'js', 'outcome', 'verify', 'dur(s)', 'req', 'aux', 'input', 'cache_r', 'cache_w', 'output', 'reason', 'cost$', 'tools', 'run_js', 'child', 'child by tool', 'rj err(js/host/to/lim)', 'rj out B', 'rj ret B', 'shell', 'prog', 'build chain', 'chars med/max'],
    runs.map((r) => {
      const e = r.run_js.errors;
      return [
        r.model ?? '-', r.task ?? '-', r.rep ?? '-', r.run_js_enabled == null ? '-' : r.run_js_enabled ? 'on' : 'off', r.outcome ?? r.status ?? '-', r.verify ?? '-', fmt(r.duration_s, 0),
        r.usage.requests, r.usage.aux_requests, r.usage.input_tokens, r.usage.cache_read_tokens, r.usage.cache_write_tokens, r.usage.output_tokens, r.usage.reasoning_tokens, fmt(r.usage.cost_usd, 4),
        r.tools.top_level_calls, r.run_js.calls, r.run_js.child_calls, compact(r.tools.child_by_tool),
        `${e.js_exception}/${e.host_rejected}/${e.timeout}/${e.limit_hit}`, r.run_js.output_bytes, r.run_js.return_bytes,
        r.shell.commands, r.shell.programmatic_cmds, r.shell.build_chain_cmds, `${fmt(r.shell.chars.median)}/${fmt(r.shell.chars.max)}`,
      ].map(String);
    }),
  );
  lines.push('## モデル × 課題（平均）', '');
  table(
    ['model', 'task', 'js', 'n', 'verify pass', 'err+pass', 'aborted', 'dur(s)', 'req', 'input', 'cache_r', 'cache_w', 'output', 'reason', 'cost$', 'tools', 'run_js', 'child', 'rj js-exc', 'rj host-rej', 'rj timeout', 'rj limit', 'rj out B', 'rj ret B', 'shell', 'prog', 'build chain', 'chars med'],
    agg.map((g) => {
      const m = g.mean;
      return [
        g.model ?? '-', g.task ?? '-', g.run_js_enabled == null ? '-' : g.run_js_enabled ? 'on' : 'off', g.runs, `${g.verify_pass}/${g.verify_known}`, g.error_but_pass, g.aborted, fmt(m.duration_s, 0),
        fmt(m.requests, 1), fmt(m.input, 0), fmt(m.cache_read, 0), fmt(m.cache_write, 0), fmt(m.output, 0), fmt(m.reasoning, 0), fmt(m.cost_usd, 4),
        fmt(m.tool_calls, 1), fmt(m.run_js_calls, 1), fmt(m.child_calls, 1), fmt(m.run_js_js_exception, 2), fmt(m.run_js_host_rejected, 2), fmt(m.run_js_timeout, 2), fmt(m.run_js_limit_hit, 2),
        fmt(m.run_js_output_bytes, 0), fmt(m.run_js_return_bytes, 0), fmt(m.shell_cmds, 1), fmt(m.programmatic_cmds, 1), fmt(m.build_chain_cmds, 1), fmt(m.chars_median, 0),
      ].map(String);
    }),
  );
  lines.push('## shell コマンドのプログラム的構造（run ごと）', '');
  table(
    ['model', 'task', 'rep', 'cmds', 'prog', 'script', 'loop', 'textproc', 'sed -i multi', 'heredoc interp/write+run/write only/other', 'build chain', 'build steps max', 'segments 1/2-3/4-7/8+', 'segments med/p90/max', 'chars med/p90/max', 'interp inline by lang', 'interp file', 'child shell'],
    runs.map((r) => {
      const s = r.shell;
      const k = s.programmatic_by_kind;
      const trio = (x) => `${fmt(x.median, 1)}/${fmt(x.p90, 1)}/${fmt(x.max, 0)}`;
      return [
        r.model ?? '-', r.task ?? '-', r.rep ?? '-', s.commands, s.programmatic_cmds, k.script, k.loop, k.textproc, k.sed_multi, `${s.heredoc_by_kind.interp}/${s.heredoc_by_kind.write_run}/${s.heredoc_by_kind.write_only}/${s.heredoc_by_kind.other}`, s.build_chain_cmds, fmt(s.build_steps.max),
        `${s.bands['1']}/${s.bands['2-3']}/${s.bands['4-7']}/${s.bands['8+']}`, trio(s.segments), trio(s.chars), compact(s.interp_inline_by_lang), s.interp_file_runs, s.child_commands,
      ].map(String);
    }),
  );
  const noted = runs.filter((r) => r.notes.length > 0);
  if (noted.length > 0) {
    lines.push('## 注意', '');
    for (const r of noted) lines.push(`- ${r.model}/${r.task}/${r.rep} (${r.run_id ?? 'no run_id'}): ${r.notes.join('; ')}`);
    lines.push('');
  }
  return lines.join('\n');
}

// ---------------------------------------------------------------- 入力

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function inspect(bin, db, args) {
  const res = spawnSync(bin, ['inspect', '--db', db, ...args], { encoding: 'utf8', maxBuffer: 512 * 1024 * 1024 });
  if (res.error || res.status !== 0) {
    throw new Error(`evorch inspect ${args.join(' ')} 失敗: ${res.error?.message ?? res.stderr.trim()}`);
  }
  return JSON.parse(res.stdout);
}

function loadFromDb(bin, db, runId) {
  if (!bin) throw new Error('--evorch か EVORCH_BIN が要る（DB を読むため）');
  return {
    run: inspect(bin, db, ['run', runId]),
    events: inspect(bin, db, ['events', '--run', runId, '--limit', '10000']),
  };
}

const isDir = (p) => {
  try {
    return fs.statSync(p).isDirectory();
  } catch {
    return false;
  }
};
const ATTEMPT_SUFFIX = /\.attempt\d+$/;

/**
 * `<root>/<model>/<task>/<rep>/result.json` を読む。結果ディレクトリの直下に余計なディレクトリ（timeline/、
 * 別の outroot、手で退避したもの）があっても、`result.json` の model / task / rep がパスと一致するものだけを取る。
 * 一致しないものは `entries.skipped` に理由を入れる。`<rep>.attempt<N>`（--retry-errors が退避した最初の試行）は
 * 既定では読まない（`includeAttempts` で読む）。
 */
export function loadResults(root, bin, { includeAttempts = false } = {}) {
  const entries = [];
  const skipped = [];
  const visible = (names) => names.filter((n) => !n.startsWith('.'));
  for (const model of visible(fs.readdirSync(root)).sort()) {
    const mdir = path.join(root, model);
    if (!isDir(mdir)) continue;
    for (const task of visible(fs.readdirSync(mdir)).sort()) {
      const tdir = path.join(mdir, task);
      if (!isDir(tdir)) continue;
      for (const repDir of visible(fs.readdirSync(tdir)).sort((x, y) => x.localeCompare(y, undefined, { numeric: true }))) {
        const dir = path.join(tdir, repDir);
        const resultFile = path.join(dir, 'result.json');
        if (!isDir(dir) || !fs.existsSync(resultFile)) continue;
        const isAttempt = ATTEMPT_SUFFIX.test(repDir);
        if (isAttempt && !includeAttempts) {
          skipped.push(`${model}/${task}/${repDir}: 退避された試行（--include-attempts で読む）`);
          continue;
        }
        const rep = repDir.replace(ATTEMPT_SUFFIX, '');
        let result;
        try {
          result = readJson(resultFile);
        } catch (error) {
          skipped.push(`${model}/${task}/${repDir}: result.json が読めない (${error.message})`);
          continue;
        }
        const expected = { model, task, rep };
        const mismatch = Object.entries(expected).filter(([k, v]) => result[k] != null && String(result[k]) !== v);
        if (mismatch.length > 0) {
          skipped.push(`${model}/${task}/${repDir}: result.json と場所が合わない (${mismatch.map(([k, v]) => `${k}=${result[k]} ≠ ${v}`).join(', ')})`);
          continue;
        }
        const entry = { label: { model, task, rep: repDir }, result };
        const runFile = path.join(dir, 'inspect-run.json');
        const eventsFile = path.join(dir, 'inspect-events.json');
        try {
          if (fs.existsSync(runFile) && fs.existsSync(eventsFile)) {
            entry.run = readJson(runFile);
            entry.events = readJson(eventsFile);
          } else if (result.run_id && fs.existsSync(path.join(dir, result.db ?? 'run.db'))) {
            Object.assign(entry, loadFromDb(bin, path.join(dir, result.db ?? 'run.db'), result.run_id));
          } else entry.error = 'inspect の出力も DB もない';
        } catch (error) {
          entry.error = String(error.message ?? error);
        }
        entries.push(entry);
      }
    }
  }
  entries.skipped = skipped;
  return entries;
}

function parseArgs(argv) {
  const args = { runs: [], format: 'md' };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${a} に値がない`);
      return argv[++i];
    };
    if (a === '--results') args.results = value();
    else if (a === '--db') args.db = value();
    else if (a === '--run') args.runs.push(value());
    else if (a === '--fixture') args.fixture = value();
    else if (a === '--evorch') args.evorch = value();
    else if (a === '--json') args.json = value();
    else if (a === '--md') args.md = value();
    else if (a === '--format') args.format = value();
    else if (a === '--include-attempts') args.includeAttempts = true;
    else if (a === '-h' || a === '--help') args.help = true;
    else throw new Error(`未知の引数: ${a}`);
  }
  return args;
}

function main() {
  let args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(error.message);
    process.exit(2);
  }
  const bin = args.evorch ?? process.env.EVORCH_BIN;
  if (args.help || (!args.results && !args.fixture && !(args.db && args.runs.length))) {
    console.error(fs.readFileSync(fileURLToPath(import.meta.url), 'utf8').split('\n').slice(1, 20).map((l) => l.replace(/^\/\/ ?/, '')).join('\n'));
    process.exit(2);
  }
  let entries = [];
  try {
    if (args.results) {
      entries = loadResults(args.results, bin, { includeAttempts: args.includeAttempts });
      for (const msg of entries.skipped) console.error(`skip ${msg}`);
      if (entries.length === 0) {
        console.error(`${args.results} に <model>/<task>/<rep>/result.json が 1 つもない。--results は run-matrix.sh の --outroot を指す（その親や timeline/ ではない）`);
        process.exit(1);
      }
    }
    else if (args.fixture) entries = readJson(args.fixture).runs;
    else {
      for (const spec of args.runs) {
        const [id, labelText] = spec.split('=');
        const [model, task, rep] = (labelText ?? '').split('/');
        entries.push({ label: labelText ? { model, task, rep } : {}, runId: id, ...loadFromDb(bin, args.db, id) });
      }
    }
  } catch (error) {
    console.error(String(error.message ?? error));
    process.exit(1);
  }
  const runs = entries.map((entry) => {
    const computed = computeRun(entry);
    if (entry.error) computed.notes.push(entry.error);
    return computed;
  });
  const agg = aggregate(runs);
  const json = JSON.stringify({ generated_at: new Date().toISOString(), runs, aggregate: agg }, null, 2);
  const md = toMarkdown(runs, agg);
  if (args.json) fs.writeFileSync(args.json, json + '\n');
  if (args.md) fs.writeFileSync(args.md, md);
  if (args.format === 'json' || args.format === 'both') console.log(json);
  if (args.format === 'md' || args.format === 'both') console.log(md);
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) main();
