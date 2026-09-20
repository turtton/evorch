# evorch

Target repo: `turtton/evorch`

AI-Native Agent Harness / Agent Workbench の intent host リポジトリ。
ソース構想: [agent-harness-concept.md](../../agent-harness-concept.md)

## Entrypoints

- [Intent Map](intent-tree/00-map.md) — ドメイン全体マップ
- [Mission](identity/mission.md) — 使命・ビジョン・原則・用語集
- [Product Overview](product/overview.md) — 製品概要・non-goals
- [Features](features/index.md) — 8 feature 領域の概要
- [Architecture](technology/architecture.md) — Agent Kernel 構成・技術スタック
- [MVP Roadmap](technology/mvp-roadmap.md) — v0.1–v0.5 ロードマップ

## 運用

このリポジトリは intent-cli の host リポジトリ。作業前に `intent-cli guide intent-work setup --kind tree-layout --domain evorch --target-repo turtton/evorch --format markdown` でガイダンスを確認する。

## 開発フロー

### 標準パス (intent-cli issue/PR フロー)

通常の execution unit は intent-cli のフローに載せる: `intent-cli packet` で packet を起案し、`issue draft` / `issue create` / `issue publish-flow` で issue を publish、worker が実装して PR を作成、マージ後に closeout する。運用コマンドの単一ソースは `intent-cli guide` / `intent-cli automation` 系。このパスを通った unit は queue-state に `linked_issue` / `linked_pr` が記録される。

### direct-main ユニット (脱出ハッチ)

小粒な機能・実験的な修正・使ってみたときの不満ベースの微調整は、issue/PR セレモニーを経ず main への直接実装を許容する (direct-main units)。これらは queue-state 上で `linked_issue` / `linked_pr` が null (未設定) のまま completed となる。現在 queue にある direct-main units (2026-09-21 時点、計 10):

- `v05-image-attachment-ui`
- `v06-model-catalog-preset`
- `v06-a-core-gui-ux` / `v06-b-role-capability-add` / `v06-c-process-owner-claim`
- `v07-d-memory-task-improvement`
- `v07-e2-team-mode`
- `v07-e4-role-eval-arena`
- `v07-browser-embedded`
- `v08-verification-matrix`

追跡性ルール: direct-main units はコミットメッセージに対応する intent の feature / ADR を明記すること (例: `docs(routing): ... ADR-0013`)。issue/PR なしでも intent との対応関係を辿れるようにする最小限の約束で、host policy の "main direct" ルールと整合する。

補足 (drift 防止): この host リポジトリでは queue 上の `completed` を終端状態として扱う。「レビュー済みか」と「証跡があるか」の区別は queue projection のスコープ外であり、コードレビュー側の責務とする。