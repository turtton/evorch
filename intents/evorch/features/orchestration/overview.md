# Feature: Orchestration（動的オーケストレーション）

[features 一覧](../) / [agent-runtime-kernel](../agent-runtime-kernel/overview.md) / [mission](../../identity/mission.md)

## 概要

workflow は固定しない。Agent の責任・認知モード・権限・実行ポリシーを固定し、Orchestrator が依頼に応じて動的に topology を構築する。

## 要件

- **Intent Gate**: task type / required capabilities / mutation allowed? / scope / uncertainty / expected output / completion criteria / likely need for delegation を抽出する。workflow は決めない
- **Execution Shape**: Direct（単純な質問・局所的修正）または Coordinated（複雑な調査・実装・並列探索）だけを決める
- **Role 分離**: Orchestrator / Explorer / Librarian / Oracle / Planner / Reviewer / Worker / Multimodal を capability boundary として分離する。cognitive isolation（生成と独立レビューの分離）を徹底する
- **Orchestrator の tool 制限**: delegate / delegate_background / send_message / wait / cancel / list_agents / inspect_agent / read / grep / git_diff / compact / finish のみ。write / edit / apply_patch / arbitrary shell / git commit は持たせない
- **DelegationValue**: Expertise / Parallelism / ContextIsolation / IndependentReview / DifferentInformationSource / Scale。「複雑だから delegate」ではなく delegation に具体的価値を要求する
- **Agent の5軸分解**: Agent Instance = Role + Category + Skills + Execution Policy + Route Policy

## 受け入れ基準

- Intent Gate が Direct / Coordinated を返し、Coordinated の場合のみ Orchestrator が起動すること
- Orchestrator に mutation tool が無いこと（runtime レベルで拒否されること）
- delegation の理由が説明可能であること

## Related decisions

- [ADR 0001: 固定 workflow を採用しない](../../decisions/0001-no-fixed-workflow.md)
- [ADR 0002: Role は capability boundary とし、prompt discipline ではなく権限で分離する](../../decisions/0002-role-capability-boundaries.md)

## Open questions

- Explorer / Librarian / Reviewer の runtime レベル capability 制限の具体設定（network の role-dependent 扱いの細部）
- Category（quick / deep / high-reasoning / visual / writing / research）のモデル routing との対応表
