## Goal

message/event textへの明白なcredential混入をstorage ingressで検出し、値を漏らさない診断とともにSQLite書込み前に拒否する。

## Why This Slice Exists Now

issue #3のv0.1 inspectで、credential列を持たない型/schemaだけでは自由文字列値へのAPI key混入を防げないmedium gapが見つかった。ADR 0008のcredential隔離をv0.1.1でdefense-in-depthとして補強する。

## Current Observed State

- `crates/storage/src/entity.rs:153-170` の `MessageRecord.content/reasoning` は無制限String。
- `crates/storage/src/repo/message.rs:10-16,31-40` は値検査なくINSERT/UPDATEする。
- eventのdelta/reasonも自由文字列 (`crates/event-bus/src/event.rs:168-199,225-230,265-277`) で、`repo/event.rs:34-45` がそのままserializeする。
- `crates/storage/tests/credential_payload.rs:100-151` はJSON key allowlistでありString valueを検査しない。

## Accepted Baseline You May Assume

- ADR 0008はcredential隔離をv0.1要件とする。
- workspaceはregex 1、serde/serde_json 1、tracing 0.1、rusqlite 0.40.2 bundled。
- 本guardはheuristicであり完全なsecret非漏洩保証ではない。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/storage/src/entity.rs`, `crates/storage/src/repo/message.rs`, `crates/storage/src/repo/event.rs`, `crates/storage/src/writer.rs`, `crates/storage/src/error.rs`, `crates/storage/tests/`, `intents/evorch/features/storage-memory/overview.md`

Target part: message/event textのcredential保存前guard

## In Scope

- explicit known credential valuesと高signal API-key shapeの検出。
- message content/reasoningとevent delta/reason fieldのmutation前検査。
- secret値を含めないtyped error/tracing診断。
- reject時のDB/accounting不変。
- positive/negative/redaction tests。
- heuristicの限界をstorage-memory overviewへ記録。

## Out Of Scope

- provider credential handling、keychain、config secret policy。
- 完全DLP/全secret形式保証、既存DBscan。
- 一般logging redaction redesign。

## Standalone Child Issue Contract

storage ingressへtest可能なsecret guardを追加し、明示注入されたknown credential valuesまたは高signalなAPI-key-shaped文字列を `MessageRecord.content/reasoning` とeventのdelta/reason fieldから検出した場合、INSERT/UPDATE/serialize/accountingより前に専用errorで拒否する。error/tracing/Debugにはsecret候補や前後contextを含めず、field/ruleだけを示す。通常文を過剰拒否しないnegative tests、代表key/known valueのpositive tests、DB不変/redaction testsを追加し、heuristicで完全保証ではないことをoverviewへ記録する。

## Acceptance Criteria

- message content/reasoningの既知値・明白key形状をcreate/update前に拒否する。
- event delta/reasonの同様の値をserialize/append前に拒否する。
- known valueを限定的かつtest可能に注入できる。
- diagnosticにsecret本体/前後contextを含めない。
- reject時にDB/accounting/session bytesが不変。
- positive/negative/redaction testsがある。
- overviewにheuristicの対象と限界を記録する。

## Verification

- `cargo test -p storage`（message/event positive、negative corpus、DB不変、診断redaction）。
- `cargo clippy -p storage --all-targets -- -D warnings` / `cargo fmt --all --check` / `git diff --check`。

## Related Links

- [storage-memory/overview.md](../../../intents/evorch/features/storage-memory/overview.md)
- [0008-threat-model-phased-adoption.md](../../../intents/evorch/decisions/0008-threat-model-phased-adoption.md)
- [0018-sqlite-storage-schema.md](../../../intents/evorch/decisions/0018-sqlite-storage-schema.md)

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs writeback is expected for this slice.

- Intent placement: 既存storage-memory / ADR 0008。
- ADR candidate: なし。
- Diagram candidate: なし。
- Docs update: storage-memory overview（必須）。
- Closeout writeback expected: yes（rule、redaction、false-positive対策、非保証範囲）。

## Guide Reachability (G645)

内部storage ingressのみでrole-facing surfaceは追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
