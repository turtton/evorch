#!/usr/bin/env bash
# 部分誤答（delete-tests）。詳細は t4-build-test-v2.sh の冒頭。
WRONG=delete-tests exec "$(dirname "$0")/t4-build-test-v2.sh" "$@"
