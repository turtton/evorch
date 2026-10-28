#!/usr/bin/env bash
# t6-debug-fix の初期状態: apply-patch に 2 つの退行を持つコミットを積む。
# 使い方: setup/t6-debug-fix.sh <clone-dir>
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
clone=${1:?usage: $0 <clone-dir>}
git -C "$clone" apply "$here/t6-debug-fix.patch"
git -C "$clone" -c user.name=eval -c user.email=eval@localhost -c commit.gpgsign=false \
  commit -q -am "apply-patch: tidy EOF search start and line-ending detection"
