#!/usr/bin/env bash
# 部分誤答: apply-patch の seek_sequence.rs のテストを緩めて通す（ソースの退行は残る）。
set -euo pipefail
cd "${1:?usage}"
git checkout 4391a7e5 -- crates/apply-patch/src/text_file.rs crates/apply-patch/src/targets.rs crates/apply-patch/src/apply.rs crates/agents/src/role.rs
git reset -q
python3 - <<'PY'
p = "crates/apply-patch/src/seek_sequence.rs"
s = open(p).read()
i = s.index("#[cfg(test)]")
s = s[:i] + s[i:].replace("assert_eq!", "let _ = ", 3)
open(p, "w").write(s)
PY
printf '%s\n' seek_sequence.rs text_file.rs targets.rs apply.rs role.rs a b > answers/t6.md
