# ADR 0030 ③: compaction のチェーン化と、モデル切り替え時の元履歴からの要約し直し

## Goal

チェーンのフォールバックや復元でプロバイダ・モデルが変わっても、文脈が失われず、context window を溢れないようにする。

## Why This Slice Exists Now

Codex 公式 compaction の結果は暗号化された blob で、Anthropic / OpenAI 互換の wire は警告だけ出して捨てる。v13-b でチェーンをまたぐ切り替えが日常的に起きるようになると、Codex で compaction したセッションが他プロバイダへ移ったときに圧縮済みの文脈が黙って消える。小さい context window のモデルへ移った場合は、そのまま溢れる。

## Current Observed State

- 要約は公式 compaction → 実行中と同じモデルでの要約、または構造的要約。別のモデルで要約する経路はない。
- `RunContext` は元のメッセージを残したまま checkpoint を重ねている。run スナップショットも両方を保存している。
- フォールバックは送信ループの中で起き、ループは checkpoint を扱えない。

## Accepted Baseline You May Assume

- blob を読めるモデルで解読する方式は採らず、checkpoint が覆う元のメッセージから要約し直す（ADR 0030）。
- 切り替えた時点で cache は失われるので、切り替え時の compaction は追加の cache 損失にならない。
- 構造的要約はモデルを使わないので必ず完了する。

## Target Repo / Path / Part

- Target repo: `turtton/evorch`
- Target paths: `crates/config/` `crates/runtime/` `crates/routing/` `crates/providers/` `docs/` `intents/`
- Target part: 要約モデルの選択、`compactor` ロール、切り替えの検出と送信前の要約し直し・compaction、復元時の適用、cache 契約のテスト。

## In Scope

- 要約モデルの選択（公式 → チェーン → 構造的要約）。
- `agents.roles.compactor` の追加。
- 切り替え先がブロックを解釈できるかの判定（プロバイダ・protocol の capability）。
- 送信ループからの「切り替えに compaction が必要」の通知と、agent loop での要約し直し・compaction・再送。
- 復元時の同じ手順の適用。
- context-engine の feature と `docs/providers.md` への記載。

## Out Of Scope

チェーンを並べ替える GUI と自動グルーピング（v13-d）。要約プロンプトの改善。

## Acceptance Criteria

1. 要約のモデルが、公式 compaction → チェーンの適格なメンバー → 構造的要約の順で選ばれる。
2. `agents.roles.compactor` を設定した場合だけ、チェーンの部分が置き換わる。
3. 切り替え先が解釈できない checkpoint は、元のメッセージから要約し直したテキストの checkpoint に置き換わり、blob は切り替え先に送られない。
4. 切り替え先の context window × 閾値を超える場合は、送信前に compaction が行われる。
5. 復元時の切り替えにも同じ手順が適用される。
6. 使えるモデルがなければ構造的要約で完了し、切り替えが止まらない。
7. 切り替え時の要約し直しが cache 契約上の明示的な境界として扱われ、その後のターンで prefix が保たれる。
8. `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更範囲の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check` が pass する。

## Verification

1. Unit: 要約モデルの選択、解釈可否の判定、元のメッセージの範囲の取り出し。
2. Integration: mock の Codex で公式 compaction → quota 失敗 → Anthropic 互換へ切り替え、送られたリクエストに blob がなくテキストの要約があること。復元時の同じ挙動。cache 契約のシナリオ。
3. Build / lint: 上記のコマンド一式。

## Related Links

- `intents/evorch/decisions/0030-role-model-chains.md`
- `intents/evorch/decisions/0003-cache-first-context-engine.md`
- `intents/evorch/features/context-engine/overview.md`

## Base Branch Policy

`main` から作業ブランチを切り、PR で `main` にマージする。
