// analyzeShell の単体確認。実行: node test/shell-analysis-test.mjs
import assert from 'node:assert/strict';
import { analyzeShell, summarize } from '../scripts/metrics.mjs';

const a = (c) => analyzeShell(c);
const langs = (c) => a(c).interpreters.map((i) => `${i.lang}:${i.mode}`);

// 基本: パイプ・連結・引用符内の演算子は数えない
assert.deepEqual([a('ls').pipes, a('ls').chain_ops, a('ls').segments_raw, a('ls').segments], [0, 0, 1, 0]);
assert.deepEqual([a('grep x f').segments_raw, a('grep x f').segments], [1, 1]);
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
// segments: cd / pwd / ls / echo / git status / which と cargo の build 系、制御語だけの区切りは数えない
const seg = (c) => a(c).segments;
assert.equal(seg('cd /x && ls -la && pwd && git status --short'), 0);
assert.equal(seg('cd /x && grep -rn foo crates | wc -l'), 2);
assert.equal(seg('echo hi; which node; git -C x status'), 0);
assert.equal(seg('git diff --stat && git status'), 1);
assert.equal(seg('cargo fmt -p x -- --check && cargo clippy -p x -- -D warnings && cargo test -p x'), 0);
assert.equal(seg('cargo test -p x 2>&1 | tail -n 20'), 1);
assert.equal(seg('for f in a b; do grep x "$f"; done'), 2); // for / grep（done は制御語）
assert.equal(seg('if [ -f x ]; then cat x; fi'), 2); // [ -f x ] / cat x（then / fi は制御語）
assert.equal(a('cd /x && ls').segments_raw, 2);
assert.deepEqual(a('cd /x && ls && cargo test').segments_excluded, { trivial: 2, build: 1, control: 0 });
// build 連結数
const bs = (c) => a(c).build_steps;
assert.equal(bs('cargo fmt -p x -- --check && cargo clippy -p x -- -D warnings && cargo test -p x'), 3);
assert.equal(bs('cargo +nightly fmt --check'), 1);
assert.equal(bs('cargo -q test --offline -p x'), 1);
assert.equal(bs('cargo run -p x'), 0);
assert.equal(bs('cargo build; cargo check'), 2);
assert.equal(bs('echo "cargo test"'), 0);
// プログラム的構造 ①ヒアドキュメント／インライン・インタプリタ
const prog = (c) => a(c).programmatic;
assert.equal(prog("python3 - <<'PY'\nprint(1)\nPY").script, true);
assert.equal(prog("node -e 'console.log(1)'").script, true);
assert.equal(prog("awk '{print $1}' f").script, true);
assert.equal(prog("perl -pe 's/a/b/' f").script, true);
assert.equal(prog('python3 x.py').script, false);
assert.equal(prog("jq '.a' f").script, false);
assert.equal(prog('git commit -F - <<EOF\nmsg\nEOF').script, true);
assert.equal(prog('grep x f | head').script, false);
// ② for / while / xargs によるファイル反復
assert.equal(prog('for f in $(git ls-files); do wc -l "$f"; done').loop, true);
assert.equal(prog('while read -r f; do echo "$f"; done < list').loop, true);
assert.equal(prog('git ls-files | xargs wc -l').loop, true);
assert.equal(prog('grep -rl foo crates').loop, false);
assert.equal(prog('echo "for x in y; do"').loop, false);
// ③ text 処理 3 段以上（head / tail は段に数えない）
assert.equal(prog('grep -rn foo crates | sort | uniq -c').textproc, true);
assert.equal(prog('grep -rn foo crates | sort | uniq -c | sort -rn | head -20').textproc, true);
assert.equal(prog('grep -rn foo crates | sort | head -20').textproc, false);
assert.equal(prog('git ls-files | grep rs | sed s/a/b/ | tail -5').textproc, false); // git は段に数えない: grep + sed の 2 段
assert.equal(prog('grep a f; sort g | uniq | wc -l').textproc, true); // 2 つ目のパイプラインは sort / uniq / wc の 3 段
assert.equal(prog('grep a f | cat | sort; uniq g').textproc, false); // パイプラインをまたいで数えない
// ④ 複数ファイルへの sed -i
assert.equal(prog("sed -i 's/a/b/' x.rs y.rs").sed_multi, true);
assert.equal(prog("sed -i 's/a/b/' x.rs").sed_multi, false);
assert.equal(prog("sed -i -e 's/a/b/' x.rs y.rs z.rs").sed_multi, true);
assert.equal(prog("sed -i.bak -E 's/a/b/' crates/*/src/lib.rs").sed_multi, true);
assert.equal(prog("git grep -l a | xargs sed -i 's/a/b/'").sed_multi, true);
assert.equal(prog("for f in a b; do sed -i 's/a/b/' $f; done").sed_multi, true);
assert.equal(prog("sed -n '1,5p' x.rs y.rs").sed_multi, false);
assert.equal(prog("sed -E -i 's/a/b/' x.rs; sed -i 's/c/d/' y.rs").sed_multi, true);

// summarize
assert.deepEqual(summarize([]), { n: 0, mean: null, median: null, p90: null, max: null });
assert.equal(summarize([1, 2, 3, 4]).median, 2.5);
assert.equal(summarize([5]).max, 5);
console.log('shell-analysis-test: ok');
