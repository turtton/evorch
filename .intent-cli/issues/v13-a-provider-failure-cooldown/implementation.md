# v13-a-provider-failure-cooldown Implementation Packet

## Goal

quota 切れや障害の起きたプロファイルに新しい run が毎回リクエストを送らないようにし、失敗の原因をイベントから読めるようにする。

## Why

チェーンを導入すると（v13-b）、quota 切れのメンバーを毎回叩いてから次へ進むコストが run ごと・メンバーごとに積み上がる。失敗の分類も、ボディが残っていないと改善できない。

## Scope

- **Event:** `ProviderEvent::RequestFailed` に `detail: Option<String>` を追加する。`observe.rs` の `emit_failed` でエラーから詳細を作り、secret-guard で秘匿化して 1 KiB に切り詰める。
- **分類:** プロバイダ種別ごとの分類規則を routing に置く。kimi-subscription の 403 は、ボディが利用上限（usage limit、quota、exceeded など）を示すとき `Quota`。
- **健全性レジストリ:** `RoutedModel` が持つ共有状態として、プロファイル単位と（プロファイル, モデル）単位の「いつまで止めるか」を保持する。失敗時に記録し、resolve / next_fallback がこれを参照して該当メンバーを飛ばす。
- **期間:** Retry-After があれば優先する。なければ `routing.cooldown` の既定値（quota・rate limit）。`Auth` は期限なし。runtime の再構成でレジストリごと作り直すため、全クールダウンが解除される。
- **観測:** クールダウンの開始を `ProviderEvent::CooldownStarted` として発行する（期限切れは参照時に判定するため終了イベントは出さない）。GUI の通知・テレメトリ・storage projection の網羅 match に追加する。
- **Docs:** `docs/providers.md`。

### 採用・却下案と理由

- クールダウンの永続化は採らない（ADR 0030）。再起動後に一度失敗すれば再び記録される。
- 失敗の詳細をイベント本文に無制限に載せる案は却下する。`max_event_bytes` と秘匿化のコストを抑えるため 1 KiB に切り詰める。

## Out of scope

ロールのチェーン、一周するフォールバック、Retry-After 待機、ピンの永続化、compaction。

## Verification

### Unit

- 分類規則（ステータスとボディ、プロバイダ種別ごと）。
- 健全性レジストリの期間計算と解除。
- Router がクールダウン中のメンバーを飛ばすこと。
- エラー詳細の秘匿化と切り詰め。

### Integration → build

- mock provider の quota 403 で、次の run が同じプロファイルにリクエストを送らないこと。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- Kimi の 403 の実際のボディは未取得。分類規則はボディの語句で判定し、詳細の保存によって実際のボディを後から確認できるようにする。
