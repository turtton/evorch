# v13-c-chain-compaction Implementation Packet

## Goal

チェーンのフォールバックや復元でプロバイダ・モデルが変わっても、文脈が失われず、context window を溢れないようにする。

## Why

Codex の compaction blob は他プロバイダで黙って捨てられる。元のメッセージは `RunContext` とスナップショットに残っているので、blob を解読せずに要約し直せる（ADR 0030）。

## Scope

- **要約モデルの選択:** `ModelSummarizer` が実行中ロールのチェーン（`compactor` 設定時はそのチェーン）から、使える状態で context window が入力の推定トークン数より大きいメンバーを選ぶ。なければ構造的要約。
- **`compactor` ロール:** `agents.roles.compactor`。要約にだけ使い、agent として起動しない。
- **解釈可否:** プロバイダ・protocol ごとに `ContentBlock::Compaction` を再生できるかを capability として持つ。
- **切り替えの通知:** 送信ループが次のメンバーを選んだとき、そのメンバーが visible messages を解釈できない、または推定トークン数が context window × 閾値を超える場合は、送信せずに型付きのエラー（切り替え先を含む）を返す。
- **agent loop:** その通知を受けたら、解釈できない checkpoint を元のメッセージの範囲から要約し直したテキストの checkpoint に置き換え、必要なら通常の compaction を行ってから、切り替え先に固定して再送する。
- **復元:** 復元時にピン先が使えず別のメンバーに移る場合も、同じ判定と手順を通す。
- **cache 契約:** 切り替え時の置き換えを明示的な境界としてシナリオテストに追加する。
- **Docs:** context-engine の feature、`docs/providers.md`。

### 採用・却下案と理由

- blob を読めるモデルで解読する案は却下する。切り替えが起きるのは多くの場合そのプロバイダが使えないときで、解読役も同時に使えない。
- 送信ループの中で compaction する案は却下する。ループは checkpoint と run の状態を持たず、責務が混ざる。

## Out of scope

チェーンを並べ替える GUI と自動グルーピング（v13-d）。要約プロンプトの改善。

## Verification

### Unit

- 要約モデルの選択（context window による絞り込み、`compactor` の上書き、構造的要約への退避）。
- 解釈可否の判定、元のメッセージの範囲の取り出し。

### Integration → build

- mock の Codex で公式 compaction → quota 失敗 → Anthropic 互換へ切り替え。送信されたリクエストに blob がなく、テキストの要約があること。
- 復元時の同じ挙動。
- cache 契約のシナリオ（切り替え後のターンで prefix が保たれる）。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。
