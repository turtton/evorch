#!/usr/bin/env bash
# 部分誤答: refs を 1 つ間違え、ranking の順序を入れ替える（それ以外は正解）。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
node -e '
const j = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
j.tools.find((t) => t.name === "read").refs.runtime_tests -= 1;
[j.ref_ranking[1], j.ref_ranking[2]] = [j.ref_ranking[2], j.ref_ranking[1]];
require("fs").writeFileSync(process.argv[2], JSON.stringify(j));
' "$here/verify/expected/t1-survey-v2.json" "${1:?usage}/answers/t1.json"
