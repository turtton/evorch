#!/usr/bin/env bash
set -euo pipefail
cd "${1:?usage}"
git checkout 4391a7e5 -- crates/apply-patch/src/seek_sequence.rs crates/apply-patch/src/text_file.rs crates/apply-patch/src/targets.rs crates/apply-patch/src/apply.rs crates/event-bus/src/ring.rs crates/event-bus/src/otel/span/span_budget.rs crates/agents/src/role.rs
python3 - <<'PY'
p = "crates/apply-patch/src/apply.rs"
s = open(p).read()
add = '''impl ChangeKind {
    /// 変更の種類を表す短い英語のラベル（`added` / `updated` / `moved` / `deleted`）。
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Added { .. } => "added",
            Self::Updated { .. } => "updated",
            Self::Moved { .. } => "moved",
            Self::Deleted { .. } => "deleted",
        }
    }
}

'''
anchor = "impl AppliedChange {\n"
s = s.replace(anchor, add + anchor, 1)
open(p, "w").write(s)
PY
cargo fmt -p apply-patch
git reset -q
printf '%s\n' \
  'crates/apply-patch/src/seek_sequence.rs: EOF 検索の開始位置が max ではなく min になっていた。' \
  'crates/apply-patch/src/text_file.rs: 挿入行の改行は最初の改行を採るのに、最後の改行で上書きしていた。' \
  'crates/apply-patch/src/targets.rs: 親ディレクトリ（..）の検査が先頭の要素しか見ていなかった。' \
  'crates/apply-patch/src/apply.rs: 削除の報告が modified の枠に入っていた。ChangeKind::label は残した。' \
  'crates/event-bus/src/ring.rs: is_full が len + 1 == capacity になっていた（1 つ早く満杯と判定）。' \
  'crates/event-bus/src/otel/span/span_budget.rs: run ごとの上限が >= ではなく >、属性数の上限が < ではなく <= になっていた。' \
  'crates/agents/src/role.rs: 小文字の "oracle" の別名が from_name から消えていた。' > answers/t6.md
