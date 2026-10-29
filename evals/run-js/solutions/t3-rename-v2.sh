#!/usr/bin/env bash
# 参照解: コメント行（行頭の空白を除いて // で始まる行）以外を単語単位で置換する。
set -euo pipefail
cd "${1:?usage}"
git grep -lwE 'SpanKey|SpanAction|drain_drops|str_attr' -- crates/event-bus | xargs sed -i -E \
  '/^[[:space:]]*\/\//! { s/\bSpanKey\b/SpanRef/g; s/\bSpanAction\b/SpanCommand/g; s/\bdrain_drops\b/take_drops/g; s/\bstr_attr\b/string_attr/g }'
