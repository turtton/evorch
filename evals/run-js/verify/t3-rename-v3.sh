#!/usr/bin/env bash
# t3-rename-v3: event-bus の識別子 3 つ（コメント行は除く）と、apply-patch のフィールド 4 つ（tools にも使用箇所）の改名。
source "$(dirname "$0")/lib/common.sh"

# 1. 改名の中身（定義、ファイル別の件数、コメント行の不変、範囲）。
node "$HERE/lib/check-rename-v3.mjs" "$CLONE" "$BASE" || exit 1

# 2. ビルドとテスト（コンパイルが型を考えた網羅と過剰な置換のなさを保証する）。
#    tools のテストは環境に依存して落ちるものが 1 件あるので、tools はテストのコンパイルまでを見る。
out=$(cargo test -p event-bus --features otel-exporter --offline 2>&1) || { echo "$out" | tail -25 >&2; fail "cargo test -p event-bus --features otel-exporter が失敗"; }
ok "cargo test -p event-bus --features otel-exporter passed=$(echo "$out" | sum_passed)"
out=$(cargo test -p apply-patch --offline 2>&1) || { echo "$out" | tail -25 >&2; fail "cargo test -p apply-patch が失敗"; }
ok "cargo test -p apply-patch passed=$(echo "$out" | sum_passed)"
out=$(cargo check -p tools --tests --offline 2>&1) || { echo "$out" | tail -25 >&2; fail "cargo check -p tools --tests が失敗"; }
ok "cargo check -p tools --tests"
