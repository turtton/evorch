#!/usr/bin/env bash
# 部分誤答: B で AppliedChange.raw まで as_written に改名する（同名の別のフィールドを巻き込む。型が合わず検証が落ちる）。
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cd "${1:?usage}"
git grep -lwE 'SpanMapper|SpanDropKind|start_run' -- crates/event-bus | xargs sed -i -E \
  '/^[[:space:]]*\/\//! { s/\bSpanMapper\b/SpanTranslator/g; s/\bSpanDropKind\b/SpanDropReason/g; s/\bstart_run\b/begin_run/g }'
git apply "$here/solutions/t3-rename-v3-b.patch"
sed -i '0,/    pub raw: PathBuf,/s//    pub as_written: PathBuf,/' crates/apply-patch/src/apply.rs
