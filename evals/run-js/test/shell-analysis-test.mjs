// analyzeShell の単体確認。実行: node test/shell-analysis-test.mjs
import assert from 'node:assert/strict';
import { analyzeShell, summarize } from '../scripts/metrics.mjs';

const a = (c) => analyzeShell(c);
const langs = (c) => a(c).interpreters.map((i) => `${i.lang}:${i.mode}`);

// 基本: パイプ・連結・引用符内の演算子は数えない
assert.deepEqual([a('ls').pipes, a('ls').chain_ops, a('ls').segments], [0, 0, 1]);
assert.deepEqual([a('grep -rn foo crates | sort | uniq -c | head').pipes, a('grep foo | wc -l').segments], [3, 2]);
assert.equal(a('echo "a | b && c; d"').pipes, 0);
assert.equal(a("echo 'a | b && c; d'").chain_ops, 0);
assert.equal(a('cd x && make || echo fail; ls').chain_ops, 3);
assert.equal(a('cargo test 2>&1 | tail -n 20').pipes, 1);
assert.equal(a('cargo test 2>&1 | tail').chain_ops, 0); // 2>&1 の & は連結ではない
assert.equal(a('cmd &> /dev/null').chain_ops, 0);
assert.equal(a('echo a >| f').pipes, 0);
// 改行区切りと行数
assert.equal(a('a\nb\nc\n').newline_seps, 2);
assert.equal(a('a\nb').lines, 2);
assert.equal(a('a \\\n  b').segments, 1);
// コメントは無視
assert.equal(a('ls # a | b').pipes, 0);
// ヒアドキュメント: 本体の | や ; は数えず、個数を数える
const hd = a("cat <<'EOF' > out.txt\nline | one; two\nEOF\nwc -l out.txt");
assert.deepEqual([hd.heredocs, hd.pipes, hd.chain_ops, hd.segments], [1, 0, 0, 2]);
assert.equal(a('cat <<-EOF\n\tx\n\tEOF\n').heredocs, 1);
assert.equal(a('cat <<A; cat <<B\n1\nA\n2\nB\n').heredocs, 2);
assert.equal(a('grep x <<< "$v"').here_strings, 1);
// インタプリタ
assert.deepEqual(langs("node -e 'console.log(1)'"), ['node:inline']);
assert.deepEqual(langs('bun -e "1+1"'), ['node:inline']);
assert.deepEqual(langs("python3 - <<'PY'\nprint(1)\nPY"), ['python:inline']);
assert.deepEqual(langs('python3 -c "print(1)"'), ['python:inline']);
assert.deepEqual(langs('python3 tools/x.py'), ['python:file']);
assert.deepEqual(langs("perl -ne 'print if /x/' f"), ['perl:inline']);
assert.deepEqual(langs("ls | awk '{print $1}'"), ['awk:inline']);
assert.deepEqual(langs('awk -f x.awk f'), ['awk:file']);
assert.deepEqual(langs("cat f | jq '.a'"), ['jq:inline']);
assert.deepEqual(langs('FOO=1 node -e "x"'), ['node:inline']);
assert.deepEqual(langs('git ls-files | xargs -n1 python3 -c "x"'), ['python:inline']);
assert.deepEqual(langs('echo "node -e x"'), []);
assert.deepEqual(langs('bun test'), []);
assert.deepEqual(langs('cargo test -p agents'), []);
assert.deepEqual(langs('cd /x && node scripts/gen.mjs'), ['node:file']);
// summarize
assert.deepEqual(summarize([]), { n: 0, mean: null, median: null, p90: null, max: null });
assert.equal(summarize([1, 2, 3, 4]).median, 2.5);
assert.equal(summarize([5]).max, 5);
console.log('shell-analysis-test: ok');
