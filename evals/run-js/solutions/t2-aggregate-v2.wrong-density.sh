#!/usr/bin/env bash
# 部分誤答: 密度の順位で tools と config を入れ替え、other_lines を 1 つ落とす（それ以外は正解）。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
node -e '
const j = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
[j.density_top5[0], j.density_top5[1]] = [j.density_top5[1], j.density_top5[0]];
j.crates.runtime.other_lines = 0;
require("fs").writeFileSync(process.argv[2], JSON.stringify(j));
' "$here/verify/expected/t2-aggregate-v2.json" "${1:?usage}/answers/t2.json"
