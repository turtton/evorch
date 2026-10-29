#!/usr/bin/env bash
# t6-debug-fix-v2 の初期状態: 3 つのコミットで 5 つの退行（apply-patch に 4 つ、agents に 1 つ）を積む。
# 使い方: setup/t6-debug-fix-v2.sh <clone-dir>
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
clone=${1:?usage: $0 <clone-dir>}
commit() {
  git -C "$clone" apply "$here/t6-debug-fix-v2-$1.patch"
  git -C "$clone" -c user.name=eval -c user.email=eval@localhost -c commit.gpgsign=false commit -q -am "$2"
}
commit a "apply-patch: tidy EOF search start and line-ending detection"
commit b "apply-patch: simplify parent-dir check and summary buckets"
commit c "agents: drop redundant lowercase oracle alias"
