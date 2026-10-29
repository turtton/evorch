#!/usr/bin/env bash
# 部分誤答（sorted-roles）。詳細は t4-build-test-v3.sh の冒頭。
WRONG=sorted-roles exec "$(dirname "$0")/t4-build-test-v3.sh" "$@"
