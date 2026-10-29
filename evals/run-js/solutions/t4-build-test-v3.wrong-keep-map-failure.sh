#!/usr/bin/env bash
# 部分誤答（keep-map-failure）。詳細は t4-build-test-v3.sh の冒頭。
WRONG=keep-map-failure exec "$(dirname "$0")/t4-build-test-v3.sh" "$@"
