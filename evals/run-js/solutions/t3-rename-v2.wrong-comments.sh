#!/usr/bin/env bash
# 部分誤答: コメント行まで置換する（コードは完全。コンパイルとテストは通る）。
set -euo pipefail
cd "${1:?usage}"
git grep -lwE 'SpanKey|SpanAction|drain_drops|str_attr' -- crates/event-bus | xargs sed -i -E \
  's/\bSpanKey\b/SpanRef/g; s/\bSpanAction\b/SpanCommand/g; s/\bdrain_drops\b/take_drops/g; s/\bstr_attr\b/string_attr/g'
