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

## 実装確定（2026-08-30、PR #36 / issue #35）

- **shape**: `crates/storage/src/entity.rs` に `pub(crate) struct SecretGuard { known_values: Vec<String> }`。`from_env()`（限定 credential env 名 `CREDENTIAL_ENV_NAMES` × 最小長 8）と `with_known_values()`（明示注入、`expect(dead_code, reason=…)` 注記付き）の 2 コンストラクタ。
- **検査点**: `check_message_record`（content/reasoning）は `repo::message::{create,update}` の INSERT/UPDATE 前。`check_event_kind`（MessageDelta/ReasoningDelta の delta + Failed/AgentRunStateChanged/ExecutionDenied/ProviderFallback/RequestCompleted.finish_reason の reason 系）は `repo::event::append_event`（serialize・accounting 更新前）と `StorageHandle::append_event`（fail-fast、writer.rs）の二層。event 側は variant 明示列挙で、将来 text field 付き variant が追加されたらここへも追加する方針がコメントで固定。
- **検出規則**: ①既知値の完全一致（既知値側を指すため、診断に前後コンテキストが紛れ込む経路を構造的に排除）②prefix 接頭辞＋最小長＋字種チェックの API key 形状（sk-/ghp_系/github_pat_/xox[baprs]-/AKIA/AIza/private key block ヘッダ/JWT 三区分 base64url）。接頭辞直前が英数字の語中一致は棄却（`ask-…`/`wordAKIA…`/`xsk-…`）。完全一致・形状とも deterministic、時刻・乱数非依存、新規 dep なし。
- **拒否表現**: `StorageError::SecretDetected { entity, field, rule: SecretRule }`。規則名のみ（`known-credential-value` / 形状ラベル）で値本体・前後コンテキスト・決定的ハッシュ fingerprint は**一切含めない**（oracle 化防止のため R1 で除去。packet AC④ の「redacted fingerprint のみ」を「非包含」で上回る解釈とし worker R2 で承認）。`SecretGuard` の Debug は既知値の個数のみ手書き表示。
- **状態不変性**: 拒否は serialize・INSERT・accounting 更新の前。repo 層テストで events 行数・EventAccounting clone 比較・sessions.total_event_bytes の不変を直接検証。
- **検証**: in-crate unit test（entity.rs 3 + repo/message.rs 3 + repo/event.rs 1）+ 外部 integration `tests/secret_guard.rs` 4 test（形状 9 種・既知 env 値・accounting/bytes 不変・negative 10 件）。`GH_TOKEN` sentinel は unsafe env set/restore を SAFETY 注記付きで使用（同一 binary 内で sentinel を共有する他テストなし）。
- **docs**: `intents/evorch/features/storage-memory/overview.md` に「ストレージ ingress の secret guard（ADR 0008 補強）」セクション（対象 field・検出 2 規則・拒否/診断方針・状態不変性・限界と非目標）を worker が追記（AC⑦ = closeout_learning write_back target 充足のため lead 側 overview 追記はなし）。
