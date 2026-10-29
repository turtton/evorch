#!/usr/bin/env bash
# 使い捨ての clone を作る: 固定コミットの履歴だけを取り込み、detached HEAD にし、課題ごとの準備を適用する。
# 他の ref（run_js の実装を含む可能性のある branch など）は取り込まない。
# 使い方: scripts/prepare-clone.sh <task-id> <dest-dir> [source-repo]
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
id=${1:?usage: $0 <task-id> <dest-dir> [source-repo]}
dest=${2:?usage: $0 <task-id> <dest-dir> [source-repo]}
src=${3:-$(git -C "$here" rev-parse --git-common-dir | xargs -I{} realpath {})}
base=4391a7e5

[ -e "$dest" ] && { echo "$dest は既にある" >&2; exit 2; }
git init -q "$dest"
full=$(git -C "$src" rev-parse --verify "$base^{commit}")
git -C "$dest" fetch -q "$src" "$full"
git -C "$dest" checkout -q --detach FETCH_HEAD
[ "$(git -C "$dest" rev-parse --short=8 HEAD)" = "$base" ] || { echo "HEAD が $base ではない" >&2; exit 2; }
mkdir "$dest/answers"
printf '/answers/\n' >> "$dest/.git/info/exclude"
if [ -x "$here/setup/$id.sh" ]; then "$here/setup/$id.sh" "$dest"; fi
echo "$dest"
