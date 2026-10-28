#!/usr/bin/env bash
# 部分誤答（観察された誤り）: test_attrs の正規表現が `#[test]` だけで `#[tokio::test…]` を数えない。
# 密度の順位も、その誤った数から作り直す。それ以外は正解。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
clone=${1:?usage}
python3 - "$here/verify/expected/t2-aggregate-v3.json" "$clone" <<'PY'
import json, re, subprocess, sys
exp = json.load(open(sys.argv[1]))
clone = sys.argv[2]
files = subprocess.run(["git", "-C", clone, "ls-files", "-z", "--", "crates"], capture_output=True).stdout.decode().split("\0")
for c in exp["crates"].values():
    c["test_attrs"] = 0
for f in files:
    if f.endswith(".rs"):
        name = f.split("/")[1]
        text = open(f"{clone}/{f}", encoding="utf-8", errors="surrogateescape").read()
        exp["crates"][name]["test_attrs"] += sum(1 for l in text.split("\n") if re.match(r"\s*#\[test\]", l))
json.dump(exp, open(f"{clone}/answers/t2.json", "w"))
PY
