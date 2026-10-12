# ADR 0030 ①: プロバイダ失敗の詳細保存・種別ごとの分類・プロファイルのクールダウン

## Goal

quota 切れや障害の起きたプロファイルに、新しい run が毎回リクエストを送らないようにする。あわせて、失敗の原因をイベントから読めるようにする。ADR 0030 の後続（ロールのチェーン、compaction）の前提となる基盤である。

## Why This Slice Exists Now

thread-124 のエスカレーション先で、Kimi サブスクリプションの quota 切れが HTTP 403（`Auth` 分類）として返り、`k3 403 → neuralwatt` が約 15 秒おきに繰り返された。レスポンスボディはどこにも残っておらず、quota 切れと認証エラーを区別できなかった。

## Current Observed State

- `RequestFailed` は `ProviderFailureKind` だけを持ち、エラーボディを持たない。途中の失敗の詳細は `tracing::warn` にしか出ず、ログファイルも出力されていない。
- `FailureKind` は 402 だけを `Quota` とし、403 は一律 `Auth`。
- プロファイルの状態を持たないため、失敗済みのプロファイルを次の run が再び最初に叩く。

## Accepted Baseline You May Assume

- イベントの保存形式は後方互換を保たなくてよい（AGENTS.md の Compatibility policy）。
- クールダウンは永続化しない（ADR 0030）。
- フォールバックの順番そのもの（一周する走査、プールの境界）は v13-b で変える。この unit では変えない。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/event-bus/` `crates/providers/` `crates/routing/` `crates/runtime/` `crates/config/` `crates/storage/` `crates/gui/` `docs/` `intents/`
- Target part: `RequestFailed` のエラー詳細、失敗の分類規則、プロファイルの健全性レジストリとクールダウン、`routing.cooldown` 設定、観測イベント。

## In Scope

- `RequestFailed` に `detail: Option<String>`（secret-guard で秘匿化、最大 1 KiB）。
- プロバイダ種別ごとの失敗分類。kimi-subscription の 403 で、ボディが利用上限を示すものを `Quota` にする。
- プロファイルの健全性レジストリ。`Quota` / `Auth` はプロファイル単位、`RateLimited` は（プロファイル, モデル）単位。Retry-After を優先する。
- resolve / next_fallback でクールダウン中のメンバーを飛ばす。
- `routing.cooldown`（quota・rate limit の既定期間。Auth は期限なし）。
- クールダウン開始の観測イベント（`CooldownStarted`。終了イベントは出さない）。GUI 側の網羅 match の追従。
- `docs/providers.md` への記載。

## Out Of Scope

ロールのチェーン、一周するフォールバック、Retry-After 待機、ピンの永続化（v13-b）。compaction の変更（v13-c）。クールダウンの永続化。

## Acceptance Criteria

1. `RequestFailed` が秘匿化済みのエラー詳細（最大 1 KiB）を持ち、credential の値を含まない。
2. kimi-subscription の 403 で、ボディが利用上限を示すものは `Quota`、それ以外の 403 は `Auth` になる。
3. `Quota` / `Auth` はプロファイル全体、`RateLimited` は（プロファイル, モデル）を決められた期間止める。Retry-After があればそれを優先する。
4. クールダウン中のメンバーは resolve / next_fallback で飛ばされ、リクエストが発生しない。全員がクールダウン中なら候補なしのエラーになる。
5. 既定期間は `routing.cooldown` で変更できる。runtime の再構成（設定の再読み込み）で `Auth` を含む全クールダウンが解除される。
6. クールダウンの開始が観測イベントとして発行され、GUI が壊れない。
7. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: 分類規則、健全性レジストリの期間計算、Router のクールダウン中メンバーの読み飛ばし、エラー詳細の秘匿化。
2. Integration: mock provider で quota 403 を返し、2 回目の run が同じプロファイルへリクエストを送らないこと。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/features/provider-routing/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
