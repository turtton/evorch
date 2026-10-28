#!/usr/bin/env bash
# 課題のプロンプトの静的な点検（指示同士・指示と verify の矛盾や、確認質問で止まる原因を避けるため）。
# 使い方: scripts/lint-prompts.sh [task-id ...]   （省略時は models.sh の EVAL_TASKS = v3）
# 点検項目:
#   1. run_js の使用を指示していない（プロンプトに run_js / JS 実行の語がない）
#   2. 「確認の質問はしないでください」と、判断・仮定を `answers/notes.md` に書く指示がある
#   3. 「変更しない」「変更してよいのは」系の制限には、`answers/` への書き込みの例外が明記されている
#   4. 検証が追跡ファイルの差分（changed_files）だけを見ていて、answers/ の書き込みで落ちない（verify に `answers/` を除く記述は不要。
#      answers/ は clone の .git/info/exclude に入っていることを prepare-clone.sh が保証する）
# 終了コード: 0 = 問題なし、1 = 問題あり
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=models.sh
. "$here/scripts/models.sh"
ids=("$@")
[ ${#ids[@]} -gt 0 ] || read -r -a ids <<<"$EVAL_TASKS"
status=0
bad() { echo "NG   $1: $2"; status=1; }
for id in "${ids[@]}"; do
  prompt=$("$here/scripts/prompt.sh" "$id") || { bad "$id" "プロンプトを取り出せない"; continue; }
  [ -n "$prompt" ] || { bad "$id" "プロンプトが空"; continue; }
  if printf '%s' "$prompt" | grep -qiE 'run_js|JS 実行|javascript'; then bad "$id" "プロンプトが run_js / JS の使用に触れている"; fi
  printf '%s' "$prompt" | grep -qF '確認の質問はしないでください' || bad "$id" "「確認の質問はしないでください」がない"
  printf '%s' "$prompt" | grep -qF 'answers/notes.md' || bad "$id" "判断・仮定を answers/notes.md に書く指示がない"
  if printf '%s' "$prompt" | grep -qE '変更しないでください|変更してよいのは|以外は変更'; then
    printf '%s' "$prompt" | grep -qE '`answers/` への書き込みは除' || bad "$id" "変更の制限に answers/ への書き込みの例外がない"
  fi
  [ -x "$here/verify/$id.sh" ] || bad "$id" "verify/$id.sh がない"
  [ -x "$here/solutions/$id.sh" ] || bad "$id" "solutions/$id.sh がない"
  [ "$status" -eq 0 ] && echo "ok   $id"
done
exit $status
