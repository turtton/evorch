#!/usr/bin/env bash
# 検証スクリプトの自己確認: 各課題で次の 3 通りを確かめる。
#   untouched : 未着手の clone                      → fail（exit 1）
#   solved    : 参照解 solutions/<id>.sh を適用     → pass（exit 0）
#   wrong-*   : 部分誤答 solutions/<id>.wrong-*.sh  → fail（exit 1）
# 使い方: scripts/self-test.sh <work-dir> [task-id ...]   （work-dir は存在しない場所）
# task-id を省くと models.sh の EVAL_TASKS（v2）。旧課題も指定できる（t1-survey など）。
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=models.sh
. "$here/scripts/models.sh"
work=${1:?usage: $0 <work-dir> [task-id ...]}
shift
ids=("$@")
[ ${#ids[@]} -gt 0 ] || read -r -a ids <<<"$EVAL_TASKS"
mkdir -p "$work"
status=0

# check <id> <label> <want-exit> <solution-script|"">
check() {
  local id=$1 label=$2 want=$3 solution=$4
  local clone="$work/$id-$label"
  "$here/scripts/prepare-clone.sh" "$id" "$clone" >/dev/null || { echo "prepare 失敗: $id $label"; status=1; return; }
  [ -z "$solution" ] || "$solution" "$clone" >"$work/$id-$label.solution.log" 2>&1 || { echo "参照解/誤答の適用に失敗: $id $label（$work/$id-$label.solution.log）"; status=1; return; }
  "$here/verify/$id.sh" "$clone" >"$work/$id-$label.log" 2>&1
  local rc=$? verdict=OK
  [ "$rc" -eq "$want" ] || { verdict=MISMATCH; status=1; }
  printf '%-18s %-26s exit=%s (期待 %s) %s\n' "$id" "$label" "$rc" "$want" "$verdict"
}

for id in "${ids[@]}"; do
  check "$id" untouched 1 ""
  check "$id" solved 0 "$here/solutions/$id.sh"
  for wrong in "$here/solutions/$id".wrong-*.sh; do
    [ -f "$wrong" ] || continue
    label=$(basename "$wrong" .sh)
    label=${label#"$id."}
    check "$id" "$label" 1 "$wrong"
  done
done
exit $status
