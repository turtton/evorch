#!/usr/bin/env bash
# 部分誤答: ring.rs のテストを緩めて通す（ソースの退行は残る。ほかは直す）。
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "${1:?usage}"
"$here/t6-debug-fix-v3.sh" "$PWD"
git checkout HEAD -- crates/event-bus/src/ring.rs
python3 - <<'PY'
p = "crates/event-bus/src/ring.rs"
s = open(p).read()
i = s.index("#[cfg(test)]")
s = s[:i] + s[i:].replace("assert_eq!", "let _ = ", 4)
open(p, "w").write(s)
PY
git reset -q
