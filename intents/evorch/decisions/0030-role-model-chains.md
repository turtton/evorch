# ADR 0030: ロール単位のモデルチェーンと同一モデルの経路プール

## Status

Accepted（2026-10-12）。利用者との設計相談で方針に合意した。[ADR 0004](0004-provider-routing-separation.md) の「current provider profile → same model / another profile → alternative logical model」というフォールバック順を、設定と実行時の挙動として具体化する。

## Context

`routing.routes.<名前>` は候補リスト（provider profile + model + reasoning_effort）で、ロールは `logical_model` で 1 つの route を指す。実際の設定では route がほぼ「同じモデルへの冗長経路」（例: `sol` = codex-work → codex-personal）として使われている。一方で、別モデルへの切り替えも同じ route に混ぜるしかなく、`kimi-k2` という名前の route に `gpt-6.1-sol` が入るように、名前と中身がずれていた。

2 つのフォールバックは性質が違う。

| | 同じモデルの冗長経路 | 別モデルへの切り替え |
|---|---|---|
| 名前 | 何であるか（モデル名） | 何に使うか（ロール） |
| セッション途中の切り替え | ほぼ無害 | prompt cache、thinking block、context window、料金が変わる |
| 推論強度 | モデルの属性ではない | ロールとモデルの組で決まる |

route から route を参照する案も検討したが、依存関係が任意のグラフになり、循環や優先順位の解釈が難しくなるため採用しない。

運用で次の問題も確認した（thread-124 のエスカレーション先、2026-10-12）。

- Kimi サブスクリプションの quota 切れは HTTP 403 で返り、`Auth` に分類される。レスポンスボディはイベントにも run にも残らず、quota 切れと認証エラーを区別できない。
- プロファイルの状態を持たないため、新しい run が毎回 quota 切れのプロファイルを最初に叩く（`k3 403 → neuralwatt` が約 15 秒おきに繰り返された）。
- session affinity はメモリ上にしかなく、再起動や設定の再構成でピンが消える。スナップショットの `selected_model` は表示にしか使われていない。
- Codex 公式 compaction の結果は暗号化された blob で、Anthropic / OpenAI 互換の wire は警告だけ出して捨てる。Codex で compaction したセッションが他プロバイダへ移ると、圧縮済みの文脈が黙って消える。

## Decision

### 2 層に固定する

- **Tier 1: 経路プール（`routing.routes`）**。同じモデルへの経路だけを並べる。候補は `profile` と `model` だけを持ち、`reasoning_effort` は持たない。速度・料金の区分（`+fast`、プロバイダ独自の `-flex` / `-fast` など）は経路の属性としてプール側に残す。コスト管理のため、ロール側では切り替えない。
- **Tier 2: ロールのチェーン（`agents.*.models`）**。Tier 1 の名前を優先順に並べ、要素ごとに `reasoning_effort` を指定する。チェーンは Tier 1 だけを参照でき、チェーンがチェーンを参照することはできない。深さは常に 2 で、循環は起きない。
- 名前付きのチェーンは作らない。チェーンの名前はロール（とカテゴリ）そのものである。ロールのセットを切り替えたい場合は既存の `role_profiles` を使う。

```toml
[[routing.routes."moonshotai/kimi-k3"]]
profile = "kimi"
model = "k3"

[[routing.routes."moonshotai/kimi-k3"]]
profile = "neuralwatt"
model = "kimi-k3"
auto = true

[agents.roles.planner]
models = [
  { route = "moonshotai/kimi-k3", reasoning_effort = "high" },
  { route = "anthropic/claude-fable-5-1", reasoning_effort = "medium" },
]
```

- `models` を省略したロールは、従来どおりロール名と同名の route を 1 要素のチェーンとして使う。
- 設定スキーマは v3 とする。v2 → v3 の移行は、各ロールの `logical_model = "x"` を `models = [{ route = "x", reasoning_effort = <x の候補の effort> }]` に書き換え、route 側の `reasoning_effort` を削除する。同じ route の候補間で effort が異なる場合や、参照先 route が同じファイルにない場合は、移行内容を警告する。

### フォールバックは一周だけ回る

チェーンは実行時に「Tier 1 の候補を連結した列 + プールの境界」に展開する。

- 位置 i で失敗したら i+1 から末尾まで試し、先頭に戻って i-1 まで試す。元の位置まで戻ったら打ち止めにして、エラーとして返す。元の位置は、失敗が一時的なもの（429・5xx・timeout・transport）だった場合に限り、一周の最後にもう一度試す。
- 各メンバーは一周につき 1 回だけ試す。クールダウン中のメンバーはリクエストを送らずに飛ばす。
- プールの境界は、この一周の中でプールの全メンバーが失敗済みかクールダウン中になったら、失敗の種類に関係なく越える。障害で作業が止まるなら冗長性を用意した意味がないためである。
- 例外として、Retry-After が短い（既定 30 秒以下）一時的な失敗でプールが尽きた場合は、プールを越える前に一度だけその時間待って再試行する。短い障害で長いセッションが高価なモデルに移るのを避けるためである。
- 成功したメンバーにピンを移し、元には戻さない。プロバイダやモデルをまたぐ切り替えは cache を壊すので、頻繁に行わない。新しいセッションはチェーンの先頭から解決する。

### プロファイルの状態とエラーの分類

- `RequestFailed` に、secret-guard で伏せて短く切り詰めたエラー詳細を残す。
- 失敗の分類はプロバイダ種別ごとの規則を持つ。Kimi サブスクリプションの 403 は、ボディが利用上限を示す場合 `Quota` とする。
- プロファイル単位のクールダウン状態を runtime 内に持つ。`Quota` はプロファイル全体、`RateLimited` は（プロファイル, モデル）単位で止める。期間は Retry-After やリセット時刻があればそれに従い、なければ設定の既定値を使う。`Auth` は設定の再読み込みまで止める。クールダウンは永続化しない。

### ピンを永続化する

- ピン（チェーン内の位置、プロファイル、具体モデル）を run のスナップショットに保存し、復元時に affinity へ戻す。
- 復元時にピン先が使えない（クールダウン中、設定から削除済みなど）場合は、復元を切り替えとして扱い、通常のフォールバックと同じ手順を通す。

### compaction はチェーンで行い、切り替え時は元の履歴から要約し直す

- 要約のモデルは、①その場のプロバイダの公式 compaction → ②実行中ロールのチェーン（使える状態で、context window が入力より大きいメンバー）→ ③構造的要約、の順で選ぶ。構造的要約はモデルを使わないので必ず完了する。
- `agents.roles.compactor` を設定した場合だけ、②をそのチェーンに置き換える。
- 切り替え（フォールバック・復元）の直前に、切り替え先が解釈できないブロック（Codex の compaction blob など）を含む checkpoint があれば、blob ではなく **その checkpoint が覆う元のメッセージ** を入力にして要約し直し、テキストの checkpoint に置き換える。元の履歴は `RunContext.messages` と run スナップショットに残っている（`crates/runtime/src/context.rs` の `apply_checkpoint`、`crates/runtime/src/restore.rs` の書き出し）。
- 切り替え後の履歴が切り替え先の context window × 閾値を超える場合は、送信前に通常の compaction を行う。
- 切り替えた時点で cache はどのみち失われるので、ここでの compaction は追加の cache 損失にならない。AGENTS.md の cache 契約上も、切り替えは明示的な境界として扱う。

### 同一モデルの自動グルーピング

- プロバイダを追加したりモデルを有効にしたりした時点で、models.dev の `canonical_model_id` を引き、同じ canonical id（と速度区分）のプールがあれば最低優先度に `auto = true` で書き込む。ロールが参照していないプールは作らない。
- models.dev のどのプロバイダとして照合するかは `ProviderTypeConfig::models_dev_provider_candidates` に従い、対応表がなければプロファイル名や一意一致で推定する。
- 同じプロファイル内で、有効なモデルのうち複数が同じ canonical id に当たる場合（例: `k3` と `k3-256k`、`kimi-k3` と `kimi-k3-flex`）は自動で入れず、利用者に選ばせる。canonical id は「同じ重み」を表すだけで、context window や料金区分までは同じとは限らないためである。
- 設定を読むたびに計算し直す方式は採らない。`kimi-for-coding` のように中身が入れ替わるエイリアスで、プールの構成が黙って変わるためである。models.dev 側で canonical id が変わったら、警告だけ出す。
- `family` は粒度が粗すぎる（`kimi-k2` に 2.5〜2.7 が含まれる）ので使わない。canonical id がないモデルは自動で入れない。
- context window はプール単位ではなく、メンバー単位で判定する。

## Consequences

- route の名前を「何であるか」、チェーンを「何に使うか」に分けられ、名前と中身のずれが解消する。
- `reasoning_effort` を設定する場所がロール側に移る。同じモデルでもロールごとに推論強度を変えられる。
- 移行は 1 ファイル単位で行うため、route と agents を別の層に書いている場合は移行内容の確認が必要になる。
- フォールバックの一周と Retry-After の待機で、全滅時にエラーが返るまでの時間が延びる。
- 実装は 4 段に分ける。①エラー詳細と分類・クールダウン、②設定 v3・ルーター・ピンの永続化、③compaction、④自動グルーピングと GUI のチェーン編集。

## Related

- [ADR 0003](0003-cache-first-context-engine.md)、[ADR 0004](0004-provider-routing-separation.md)、[ADR 0013](0013-model-catalog.md)、[ADR 0014](0014-config-architecture.md)、[ADR 0027](0027-restore-contract.md)
- [features/provider-routing](../features/provider-routing/overview.md)
- [features/context-engine](../features/context-engine/overview.md)
