#!/usr/bin/env bash
# 部分誤答: A だけ行い、B（apply-patch のフィールドの改名）をしない。
set -euo pipefail
cd "${1:?usage}"
git grep -lwE 'SpanMapper|SpanDropKind|start_run' -- crates/event-bus | xargs sed -i -E \
  '/^[[:space:]]*\/\//! { s/\bSpanMapper\b/SpanTranslator/g; s/\bSpanDropKind\b/SpanDropReason/g; s/\bstart_run\b/begin_run/g }'
