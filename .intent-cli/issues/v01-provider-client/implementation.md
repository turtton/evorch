# v01-provider-client Implementation Packet

## Goal

v0.1 用の provider 抽象を `crates/providers/`（module: `anthropic` / `openai` / `openai-compatible`）に実装する。Agent Kernel から provider を透過的に扱う統一 `ProviderClient` trait（非同期 `send` / `stream` の両対応）と、OpenAI / Anthropic / OpenAI-compatible の3実装を追加する。メッセージは provider 非依存の canonical 形式（role / content / tool_calls 等）に正規化し、wire 形式（OpenAI chat.completions・Anthropic Messages API・OpenAI-compatible chat.completions）との双方向変換を持つ。ストリーミングは reqwest の SSE で受信して delta を結合し、Usage（input / output / cache read / cache write token 数）を event stream へ usage イベントとして emit する。

## Why

mvp-roadmap の v0.1 は「Provider: OpenAI / Anthropic / OpenAI-compatible」で最小構成を確定しており、agent 実行の第一歩が provider 呼び出しである。ADR 0004 は provider type / profile / logical model / API protocol の4層分離を決定済みで、本 slice がその抽象（trait + 変換 + API クライアント）を最初に実装する。また ADR 0015 第1層（CI mock 契約テスト）を満たす最初の slice であり、後続の routing / event-stream / context-engine が依存する土台となる。同一 provider 型に複数 profile を作れるようにするため、credential は本 slice では持たず認証情報は引数で注入する。

## Scope

- 統一 `ProviderClient` trait: `send`（非ストリーム）/ `stream`（SSE）を提供し、`ProviderCapabilities`（prompt_cache / reasoning / tool_calling / compaction / streaming / transport の一部。architecture.md 参照）を返す
- canonical Message 型（role / content / tool_calls / reasoning 等）と、OpenAI ↔ Anthropic 相互変換。OpenAI-compatible は chat.completions wire を openai と共用する実装にする
- reqwest による SSE ストリーミング（delta 結合、finish reason、stream error event の処理）
- Usage parse と event stream への usage イベント emit（v01-event-stream のイベント型へ接続）
- typed error（HTTP 4xx / 5xx / 429 + Retry-After / timeout / 不正 SSE）
- wiremock による mock 契約テスト（recorded response fixture。ADR 0015 第1層。CI で実 API へアクセスしない）

## Out of scope

- subscription 系 provider（openai-codex / github-copilot / anthropic-subscription）— re-evaluation-2026-08.md §1 のとおり v0.3（正規 OAuth / device code フローを含む）
- openrouter 等その他 provider type、routing / fallback / session affinity / cooldown（v01-routing-profiles）
- credential 保存・設定（v01-sandbox-approval。trait は認証情報を引数で受け取るのみ）
- model catalog（ADR 0013）、コスト計算本体（ADR 0012。本 slice は usage イベントの供給源）

## Verification

- `cargo test`: wiremock で mock server を立て、3 provider の `send` / `stream` が recorded fixture に対して正しく parse・結合できること（ADR 0015 第1層）
- canonical message 変換の round-trip テスト（OpenAI↔canonical↔Anthropic）
- usage イベントが event stream に期待形状で emit されること
- `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/provider-routing` overview を primary intent とする。v0.1 3種確定は roadmap の Open question を解消するので closeout で記録する
- ADR candidate: **あり** — canonical 正規化形式の選択は provider 横断・後戻りしにくいため ADR 0016（統一 canonical message 正規化と変換）として記録する
- Diagram candidate: decline — 変換詳細は ADR とコードコメントで十分であり、概念図の変更は不要
- Docs update: decline — 本 slice は role-facing surface（CLI / GUI / 対話 surface）を持たない。将来 GUI の provider 設定画面や debug CLI が出た時点で docs を更新する
- Closeout learning: ADR 0016 の新設と、provider-routing overview / mvp-roadmap への記録を write back する。`write_back_required: true`

- Guide reachability (G645): 本 slice は内部 crate（crates/providers/）のみを変更し、role-facing guide surface を追加しないため `no_role_facing_surface: true` を宣言する（stalled-work が route 未宣言として報告しないよう明示）

`improve` (G456 / G460) は later safety net。packet-time で上記を宣言済み。