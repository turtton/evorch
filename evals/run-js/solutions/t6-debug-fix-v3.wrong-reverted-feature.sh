#!/usr/bin/env bash
# 部分誤答: 7 ファイルを固定コミットの状態へ戻し、新機能 ChangeKind::label まで消す（コミット全体を revert したのと同じ）。
set -euo pipefail
cd "${1:?usage}"
git checkout 4391a7e5 -- crates/apply-patch/src/seek_sequence.rs crates/apply-patch/src/text_file.rs crates/apply-patch/src/targets.rs crates/apply-patch/src/apply.rs crates/event-bus/src/ring.rs crates/event-bus/src/otel/span/span_budget.rs crates/agents/src/role.rs
git rm -q -f crates/apply-patch/tests/label.rs
git reset -q
printf '%s\n' seek_sequence.rs text_file.rs targets.rs apply.rs ring.rs span_budget.rs role.rs > answers/t6.md
