#!/usr/bin/env bash
# 部分誤答: A でコメント行まで置換する（コードは完全。コンパイルとテストは通る）。B は正しく行う。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cd "${1:?usage}"
git grep -lwE 'SpanMapper|SpanDropKind|start_run' -- crates/event-bus | xargs sed -i -E \
  's/\bSpanMapper\b/SpanTranslator/g; s/\bSpanDropKind\b/SpanDropReason/g; s/\bstart_run\b/begin_run/g'
git apply "$here/solutions/t3-rename-v3-b.patch"
