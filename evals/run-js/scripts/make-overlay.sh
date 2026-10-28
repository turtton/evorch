#!/usr/bin/env bash
# モデル別の user-config overlay を作る（Worker のモデルだけが違う設定）。
#   <outdir>/config.toml                 利用者の config.toml のコピー（秘密値は入っていない。資格情報は keyring）
#   <outdir>/config.d/zz-worker.toml     [agents.worker] models = [{ route, reasoning_effort }]
# 使い方: scripts/make-overlay.sh <route> <effort> <outdir>
# 元の config.toml: $EVORCH_CONFIG_SRC（既定 ${XDG_CONFIG_HOME:-$HOME/.config}/evorch）/config.toml
# 生成物はリポジトリにコミットしない（このリポジトリの作業ツリー内への出力は拒否する）。
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
route=${1:?usage: $0 <route> <effort> <outdir>}
effort=${2:?usage: $0 <route> <effort> <outdir>}
outdir=${3:?usage: $0 <route> <effort> <outdir>}
src_dir=${EVORCH_CONFIG_SRC:-${XDG_CONFIG_HOME:-$HOME/.config}/evorch}

valid='^[A-Za-z0-9._+-]+$'
[[ $route =~ $valid ]] || { echo "route が不正: $route" >&2; exit 2; }
[[ $effort =~ $valid ]] || { echo "effort が不正: $effort" >&2; exit 2; }
[ -f "$src_dir/config.toml" ] || { echo "$src_dir/config.toml がない（EVORCH_CONFIG_SRC で指定できる）" >&2; exit 2; }

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
printf '[agents.worker]\nmodels = [{ route = "%s", reasoning_effort = "%s" }]\n' "$route" "$effort" \
  > "$outdir/config.d/zz-worker.toml"
echo "$outdir"
