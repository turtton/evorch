#!/usr/bin/env bash
# モデル別の user-config overlay を作る（Worker のモデルと provider だけが違う設定）。
#   <outdir>/config.toml                 利用者の config.toml のコピー（秘密値は入っていない。資格情報は keyring）
#   <outdir>/config.d/zz-worker.toml     評価専用 route `eval-pinned` と [agents.worker] の割り当て
# 使い方: scripts/make-overlay.sh <profile> <model-id> <effort> <outdir>
#
# provider の固定: 利用者の config.toml の route（例 gpt-luna+fast）は候補を [codex-work, codex-personal]
# の順に持ち、先頭の *-work に解決されてしまう。そこで zz-worker.toml に候補が 1 件だけの route
# `eval-pinned`（profile と model を明示）を定義して Worker に割り当てる。候補が 1 件なので他の profile
# へのフェイルオーバーも起きない。profile は subscription の codex-personal / claude-personal のみ許可する。
# 元の config.toml: $EVORCH_CONFIG_SRC（既定 ${XDG_CONFIG_HOME:-$HOME/.config}/evorch）/config.toml
# 生成物はリポジトリにコミットしない（このリポジトリの作業ツリー内への出力は拒否する）。
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
usage="usage: $0 <profile> <model-id> <effort> <outdir>"
profile=${1:?$usage}
model=${2:?$usage}
effort=${3:?$usage}
outdir=${4:?$usage}
src_dir=${EVORCH_CONFIG_SRC:-${XDG_CONFIG_HOME:-$HOME/.config}/evorch}

valid='^[A-Za-z0-9._+-]+$'
[[ $profile =~ ^(codex|claude)-personal$ ]] \
  || { echo "profile は codex-personal か claude-personal のみ（*-work や API キーの profile は使わない）: $profile" >&2; exit 2; }
[[ $model =~ $valid ]] || { echo "model-id が不正: $model" >&2; exit 2; }
[[ $effort =~ $valid ]] || { echo "effort が不正: $effort" >&2; exit 2; }
[ -f "$src_dir/config.toml" ] || { echo "$src_dir/config.toml がない（EVORCH_CONFIG_SRC で指定できる）" >&2; exit 2; }
grep -q "^\[providers\.$profile\]" "$src_dir/config.toml" \
  || { echo "$src_dir/config.toml に [providers.$profile] がない" >&2; exit 2; }

existed=0; [ -e "$outdir" ] && existed=1
mkdir -p "$outdir"
outdir=$(cd "$outdir" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel 2>/dev/null || true)
if [ -n "$repo" ]; then
  case "$outdir/" in
    "$repo"/*)
      [ "$existed" -eq 1 ] || rmdir "$outdir" 2>/dev/null
      echo "overlay をリポジトリ内($repo)に作ってはいけない: $outdir" >&2
      exit 2 ;;
  esac
fi

mkdir -p "$outdir/config.d"
cp "$src_dir/config.toml" "$outdir/config.toml"
chmod 600 "$outdir/config.toml"
printf '[[routing.routes.eval-pinned]]\nprofile = "%s"\nmodel = "%s"\n\n[agents.worker]\nmodels = [{ route = "eval-pinned", reasoning_effort = "%s" }]\n' \
  "$profile" "$model" "$effort" > "$outdir/config.d/zz-worker.toml"
echo "$outdir"
