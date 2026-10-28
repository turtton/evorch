#!/usr/bin/env bash
# 参照解: A は event-bus のコード行（コメント行を除く）を単語単位で置換、B はコンパイルエラー駆動で作った差分を適用する。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cd "${1:?usage}"
git grep -lwE 'SpanMapper|SpanDropKind|start_run' -- crates/event-bus | xargs sed -i -E \
  '/^[[:space:]]*\/\//! { s/\bSpanMapper\b/SpanTranslator/g; s/\bSpanDropKind\b/SpanDropReason/g; s/\bstart_run\b/begin_run/g }'
git apply "$here/solutions/t3-rename-v3-b.patch"
