#!/usr/bin/env bash
# t1-survey-v2: answers/t1.json を期待値と照合し、ソースが変更されていないことを確かめる。
source "$(dirname "$0")/lib/common.sh"

[ -f answers/t1.json ] || fail "answers/t1.json がない"
[ -z "$(changed_files)" ] || fail "追跡ファイルが変更されている: $(changed_files | tr '\n' ' ')"
node "$HERE/lib/check-json.mjs" t1v2 answers/t1.json "$HERE/expected/t1-survey-v2.json"
