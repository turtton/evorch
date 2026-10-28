#!/usr/bin/env bash
# 部分誤答（label）。詳細は t4-build-test-v3.sh の冒頭。
WRONG=label exec "$(dirname "$0")/t4-build-test-v3.sh" "$@"
