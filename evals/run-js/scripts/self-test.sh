#!/usr/bin/env bash
# 検証スクリプトの自己確認: 各課題で「未着手の clone は fail」「参照解を適用した clone は pass」を確かめる。
# 使い方: scripts/self-test.sh <work-dir> [task-id ...]   （work-dir は存在しない場所）
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
work=${1:?usage: $0 <work-dir> [task-id ...]}
shift
ids=("$@")
[ ${#ids[@]} -gt 0 ] || ids=(t1-survey t2-aggregate t3-rename t4-build-test t5-single-edit t6-debug-fix)
mkdir -p "$work"
status=0
for id in "${ids[@]}"; do
  for state in untouched solved; do
    clone="$work/$id-$state"
    "$here/scripts/prepare-clone.sh" "$id" "$clone" >/dev/null || { echo "prepare 失敗: $id"; status=1; continue; }
    [ "$state" = solved ] && "$here/solutions/$id.sh" "$clone"
    "$here/verify/$id.sh" "$clone" >"$work/$id-$state.log" 2>&1
    rc=$?
    want=1; [ "$state" = solved ] && want=0
    if [ "$rc" -eq "$want" ]; then verdict=OK; else verdict=MISMATCH; status=1; fi
    printf '%-16s %-9s exit=%s (期待 %s) %s\n' "$id" "$state" "$rc" "$want" "$verdict"
  done
done
exit $status
