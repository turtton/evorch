#!/usr/bin/env bash
# 部分誤答（no-grant）。詳細は t4-build-test-v2.sh の冒頭。
WRONG=no-grant exec "$(dirname "$0")/t4-build-test-v2.sh" "$@"
