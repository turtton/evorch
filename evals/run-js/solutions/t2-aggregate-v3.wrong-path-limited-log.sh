#!/usr/bin/env bash
# 部分誤答: `git log -200 -- crates/<crate>`（パスで絞ってから 200 件）で commits_200 を数える。
# 「全体の最新 200 コミットのうち crate に触れたもの」とは別の値になる。それ以外は正解。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
clone=${1:?usage}
python3 - "$here/verify/expected/t2-aggregate-v3.json" "$clone" <<'PY'
import json, subprocess, sys
exp = json.load(open(sys.argv[1]))
clone = sys.argv[2]
for name, h in exp["history"].items():
    out = subprocess.run(["git", "-C", clone, "log", "-200", "--format=%H", "4391a7e5", "--", f"crates/{name}"], capture_output=True).stdout.decode()
    h["commits_200"] = len(out.split())
json.dump(exp, open(f"{clone}/answers/t2.json", "w"))
PY
