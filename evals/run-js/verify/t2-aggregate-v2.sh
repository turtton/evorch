#!/usr/bin/env bash
# t2-aggregate-v2: answers/t2.json を期待値（固定コミットから算出）と照合する。
source "$(dirname "$0")/lib/common.sh"

[ -f answers/t2.json ] || fail "answers/t2.json がない"
[ -z "$(changed_files)" ] || fail "追跡ファイルが変更されている: $(changed_files | tr '\n' ' ')"
node "$HERE/lib/check-json.mjs" t2v2 answers/t2.json "$HERE/expected/t2-aggregate-v2.json"
