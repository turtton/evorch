#!/usr/bin/env bash
# t6-debug-fix-v3 の初期状態: 4 つのコミットで 8 つの退行（apply-patch に 4、event-bus に 3、agents に 1）を積む。
# コミット c は新機能（ChangeKind::label と tests/label.rs）も含む（コミット全体を revert すると新機能が消える）。
# 使い方: setup/t6-debug-fix-v3.sh <clone-dir>
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
clone=${1:?usage: $0 <clone-dir>}
commit() {
  git -C "$clone" apply "$here/t6-debug-fix-v3-$1.patch"
  git -C "$clone" add -A
  git -C "$clone" -c user.name=eval -c user.email=eval@localhost -c commit.gpgsign=false commit -q -m "$2"
}
commit a "apply-patch: tidy EOF search start and line-ending detection"
commit b "event-bus: tidy ring fullness check and span admission bounds"
commit c "apply-patch: add ChangeKind::label, simplify parent-dir check and summary buckets"
commit d "agents: drop redundant lowercase oracle alias"
