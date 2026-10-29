#!/usr/bin/env bash
# 部分誤答: execution_mode を上書きしていないツールを Shared と決めつけ（実際のトレイトの既定は Exclusive）、overrides を 1 件落とす。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
node -e '
const j = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
for (const t of j.tools) if (!t.overrides.includes("execution_mode")) t.execution_mode = "Shared";
j.tools.find((t) => t.name === "shell").overrides.pop();
require("fs").writeFileSync(process.argv[2], JSON.stringify(j));
' "$here/verify/expected/t1-survey-v3.json" "${1:?usage}/answers/t1.json"
