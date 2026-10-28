#!/usr/bin/env bash
# 部分誤答: 単語境界なしで str_attr を置換し、str_attribute まで壊す（コメント行は不変）。
set -euo pipefail
cd "${1:?usage}"
git grep -lwE 'SpanKey|SpanAction|drain_drops|str_attr' -- crates/event-bus | xargs sed -i -E \
  '/^[[:space:]]*\/\//! { s/\bSpanKey\b/SpanRef/g; s/\bSpanAction\b/SpanCommand/g; s/\bdrain_drops\b/take_drops/g; s/str_attr/string_attr/g }'
