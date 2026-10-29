#!/usr/bin/env bash
# t3-rename-v2: event-bus 内の 4 つの改名の網羅性・コメント行の不変・変更範囲・テストの合格を確かめる。
source "$(dirname "$0")/lib/common.sh"

# 1. 改名の中身（ファイル別の件数、コメント行の不変、範囲）。
node "$HERE/lib/check-rename-v2.mjs" "$CLONE" "$BASE" || exit 1

# 2. event-bus の外に旧名を持ち込んでいない（元から 0 件）。
outside=$(git grep -lwE 'SpanRef|SpanCommand|take_drops|string_attr' -- crates ':!crates/event-bus' || true)
[ -z "$outside" ] || fail "event-bus の外に新名が現れた: $outside"

# 3. ビルドとテスト（otel-exporter の feature を有効にして span_exporter.rs も含める）。
out=$(cargo test -p event-bus --features otel-exporter --offline 2>&1) || { echo "$out" | tail -25 >&2; fail "cargo test -p event-bus --features otel-exporter が失敗"; }
ok "cargo test -p event-bus --features otel-exporter passed=$(echo "$out" | sum_passed)"
