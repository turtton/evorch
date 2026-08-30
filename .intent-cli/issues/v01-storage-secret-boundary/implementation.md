# v01-storage-secret-boundary Implementation Packet

## Goal

message/eventの自由文字列をSQLiteへ書く直前にcredential検出guardを適用し、既知credential値または明白なAPI-key-shaped文字列を含むrecordを拒否する。診断は検出箇所を説明しつつ候補値を絶対に出力せず、この層をheuristicなdefense-in-depthとして文書化する。

## Why

issue #3 / `v01-session-storage` のv0.1 inspectで、schema column名にcredentialを持たないだけでは値混入を防げないmedium gapが判明した。`MessageRecord.content` / `reasoning` は無制限String (`crates/storage/src/entity.rs:153-170`) で、`repo/message.rs:10-16,31-40` は検査なく保存する。event側も `MessageDelta` / `ReasoningDelta` やreason fieldが自由文字列 (`crates/event-bus/src/event.rs:168-199,225-230,265-277`) で、`repo/event.rs:34-45` は全payloadをそのままJSON化する。既存test (`crates/storage/tests/credential_payload.rs:100-151`) はkey名だけを検査するため、ADR 0008のcredential隔離をv0.1.1で値境界まで補強する。

## Scope

- storage内に小さくtest可能なsecret guardを実装する。入力はtext fieldと、起動時に明示注入したknown credential values（または限定された既知credential env名から収集した値）。空/極短値はknown-value matcherへ登録しない。
- API-key-shaped検出は高signalな形状に限定する（既知prefix + 十分な長さ/entropy等）。genericな長い英数字だけで通常文章を拒否しない。
- `MessageRecord.content` と `reasoning` のcreate/updateをmutation前に検査する。後続writer-boundary後の正規writer command入口にも適用し、低水準repoが残るなら二重防御する。
- eventのhuman-readable text fieldを明示的に列挙して検査する: Message/Reasoning delta、Lifecycle failure/state reason、Tool execution-denied reason、Provider fallback reason。識別子/provider/model等への拡張は誤検知根拠がない限り行わない。
- 検出時は専用 `StorageError` を返す。entity/field/rule idは出してよいが、matched text、known value、前後contextはerror/tracing/Debugへ含めない。必要なら非可逆fingerprintのみ。
- rejectはINSERT/UPDATE/serialize/accounting更新より前に行いDBを不変にする。
- positive/negative/redaction testsを追加し、known env/valueはテスト専用に注入可能にしてprocess-global env競合を避ける。
- storage-memory overviewへheuristicの対象・限界・診断方針を追記する。

## Out of scope

- provider/client側のcredential取得・refresh・header付与。
- keychain / 0600 fallback / config credential referenceの変更。
- 完全なDLP、entropy scanner、任意secret形式の保証。検出されない形式とfalse negativeは残り得る。
- 保存済みDBの遡及scan/cleanup。
- log sink全般のredaction redesign。

## Verification

- message content/reasoningの既知値・代表key形状をcreate/updateで拒否し、rowが作成/変更されないこと。
- MessageDelta/ReasoningDeltaと各reason fieldのsecretをevent append前に拒否し、events/accounting/session bytesが不変なこと。
- error Display/Debugとcaptured tracingにsecret値/前後contextが含まれないこと。
- 通常文、UUID、短いtoken、model/provider名等のnegative corpusが受理されること。
- `cargo test -p storage` / `cargo clippy -p storage --all-targets -- -D warnings` / `cargo fmt --all --check` / `git diff --check`。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: storage-memory + ADR 0008の既存credential境界を補強。新規node不要。
- ADR candidate: なし。完全保証ではないheuristic実装詳細。
- Diagram candidate: なし。
- Docs update: storage-memory overviewに対象field、拒否/診断、限界を追記（必須）。
- Closeout learning: rule set、known-value注入、false-positive対策、redaction、非保証範囲をwrite back。`write_back_required: true`。
- Guide reachability (G645): 内部ingress guardのみでrole-facing surfaceなし。`no_role_facing_surface: true`。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
