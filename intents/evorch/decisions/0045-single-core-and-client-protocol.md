# ADR 0045: コアを 1 プロセスにし、GUI・Web・CLI を共通 protocol のクライアントにする

## Status

Proposed

## Context

[ADR 0005](0005-headless-kernel-and-gui-separation.md) および mission 原則 6（GUI は Headless Agent Kernel の上に乗るだけ）では、Agent Kernel を headless な中核とし、GUI はイベントストリームを購読する設計を掲げていた。しかし現在の実装では、`crates/gui` が `runtime`・`storage`・`sandbox` など 16 の内部 crate に直接依存しており、各 GUI プロセスが個別に runtime を内蔵している（`bin/evorch-gui.rs` が composition root となっている）。

この構成により、複数 GUI 間の協調において以下の複雑さが生じている。

- **所有権と調停の複雑さ**: [ADR 0024](0024-process-owner-claim.md) で導入された `owners.db`、プロセス間 Unix domain socket、heartbeat、claim/handoff、generation fence、`readonly_threads`。
- **表示同期と競合**: 共有 DB ポーリングによる表示同期（`shared_feed`）、および `sidebar.json` の 3-way merge。
- **高負荷時の障害実績**: 高負荷環境下で `owners.db` のロック競合により run が Error 終了し、終端イベント欠落によって GUI 上で実行中表示が残る問題が発生した（ADR 0024 追記参照）。
- **プロセス独立の利点の形骸化**: 「個別の GUI を再起動して最新版に更新できる」という利点は、頻繁な SQLite スキーマ変更により結局すべての GUI プロセスの停止が必要となるため、実際には薄れている。

さらに利用形態として、外出先や離席時にスマートフォンから Tailscale 経由で evorch を操作したいという強い要求がある（チャット返信、`ask_user` への質問回答、ツール実行承認など）。

先行するシステム設計として以下を参考にする。
- **OpenCode**: `serve` と `attach` / `web` の分離、HTTP + SSE による接続、`127.0.0.1` へのローカルバインド。
- **Codex app-server**: read / resume / subscribe / unsubscribe の分離、購読者ゼロになったスレッドのアンロード機構。

これらを踏まえ、runtime・storage・sandbox を単一のコアプロセスに集約し、GUI・Web・CLI を同一の共通 protocol を介したクライアントとするアーキテクチャへ移行する。

## Decision

### 1. ユーザー設定ディレクトリごとの単一コアプロセス

コアプロセス（`evorch-core`）は、ユーザー（設定ディレクトリ＝DB の配置場所）単位で 1 つだけ起動する。

- コアプロセスは `runtime`、`storage`、`sandbox`、`credential`、`secret-guard` を一元的に保持する。
- 同一設定ディレクトリ配下の全プロジェクト・全スレッド・全 run は、同一のコアプロセスが管理・実行する。
- 設定ディレクトリ（`EVORCH_CONFIG_DIR` 等）が異なれば別コアとして起動できる。これにより、開発中の試験ビルドや別環境を互いに干渉せず並行して動かせる。

### 2. ライフサイクル（オンデマンド自動起動とアイドル終了）

コアプロセスは常時常駐サービス（systemd 等）とはしない。最初のクライアントが必要としたときにオンデマンドで自動起動し、不要になれば自動終了する。

- **起動契機**: 最初のクライアント（デスクトップ GUI、または Web 配信設定が有効な場合のクライアント）の起動時に、コアプロセスが動いていなければ別プロセスとして自動起動する。
- **単一インスタンス保証**: ロックファイルと Unix domain socket により単一起動を保証する。ソケットファイルの存在のみで生存を判定せず、接続試行と readiness 応答を確認して判定する。
- **維持と終了**: 実行中の作業（turn、tool 実行、待機中の lane 等）が存在する間はコアプロセスを維持する。接続クライアント数が 0 かつ実行中作業がないアイドル状態が一定時間継続した場合に自動終了する。

### 3. クライアントとしてのデスクトップ GUI

デスクトップ GUI（egui 描画・GPU 処理・CEF browser）はコア外のクライアントプロセスとする。

- GUI プロセスを閉じても、コア側で進行中の作業は中断されず継続する。
- GUI プロセスの再起動や更新を、実行中のエージェント作業を停止させることなく行える。
- GPU や CEF のクラッシュがエージェントの作業コンテキストを破壊しない。

### 4. 共通 protocol の定義と通信方式

すべてのクライアント（Desktop GUI、Web、CLI）は、フレームワークや runtime に依存しない serde 可能な共通 protocol を用いてコアと通信する。

- **メッセージ構成**: `Command`、`Query`、`Snapshot`、`UiEvent`、および構造化エラーから成る。
- **機能交換**: 接続確立時に protocol version と capability のハンドシェイクを行う。
- **統一ハンドラ**: デスクトップ GUI からの操作も Web からの操作も、コア内の同一の host handler および検証ロジックを通過する。
- **内部型の非公開**: raw `Event` や runtime / storage の内部型は外部 protocol に直接公開せず、クライアント向けの安定した DTO に変換して配信する。
- **Transport**:
  - デスクトップ GUI / CLI: Unix domain socket を使用する。
  - Web UI: HTTP（Command / Query 用）+ Server-Sent Events（SSE、UiEvent 配信）を使用する。
- **SSE の整合性と本文配信**:
  - SSE はメッセージの重複を許容する設計とする。欠落時は `Last-Event-ID` とコア側の bounded replay バッファ、または snapshot による再同期を行い、subscribe と snapshot の間に隙間を作らない。
  - メッセージ本文（body）は確定分のみを配信する（token streaming のクライアント配信は後続段階で検討）。

### 5. Web UI とリモート操作のセキュリティ

Web UI はデスクトップ GUI の別表現であり、機能制限は設けない。

- **操作権限**: ユーザー入力、質問回答、ツール実行承認をデフォルトで有効とする。
- **配信設定**: コアの Web 配信機能自体は設定により明示的にオン/オフを切り替えられる。
- **ネットワーク境界**: コアの HTTP/SSE サーバーは `127.0.0.1` のみにバインドする。外部からのアクセスは Tailscale（Tailscale Serve 等）による安全なトンネリングを前提とする。
- **初期セキュリティ**: 初期段階では独自認証を設けず、Tailscale 等のネットワーク層でのアクセス制限に委ねる（後日、トークン認証等を別途検討）。ただし、ブラウザ経由の攻撃を防ぐため、CSRF 対策および DNS rebinding 対策として `Origin` ヘッダーおよび `Host` ヘッダーの厳格な検証を行う。
- **出力保護**: Web クライアントへのイベント・本文配信時にも、既存の `secret-guard`（秘密情報マスク）をデスクトップ同様に適用する。

### 6. Provenance とマルチクライアント協調

回答や承認のトレーサビリティおよび複数クライアントからの操作の整合性を保つ。

- **Provenance 記録**: 質問回答およびツール承認の provenance には、行為者種別（Human または Agent）とインターフェース種別（Desktop または Web）を記録する。端末固有情報や IP アドレスはプライバシー保護のため記録しない。
- **質問の扱い**: [ADR 0044](0044-lane-threads.md) の lead 宛て質問の扱いは、Web でもデスクトップ GUI と同様に処理する。
- **同時送信の競合**: 複数クライアントから同一スレッドへ同時にユーザー入力が送信された場合は、現行 GUI における同時送信と同様、コア側のメッセージキュー到着順で直列化して受け付ける。

### 7. ADR 0024 所有権モデルの置換

コアプロセスへの一元化に伴い、複数プロセス間の所有権機構を撤去する。

- **撤去対象**: 複数プロセス間の `owner` / `claim` / `handoff`、`owners.db`、`readonly_threads`、共有 DB ポーリング同期（`shared_feed`）、および `sidebar.json` の 3-way merge は、コア移行完了後に完全に撤去する。
- **Event Bus の位置づけ**: [ADR 0017](0017-event-bus-transport.md) で確定した in-process tokio broadcast の Event Bus は、「コアプロセス内」に適用範囲を限定して維持する。クライアントへは共通 protocol の `UiEvent` として配信する。
- **ADR 0044 Phase 1c との統合**: lane における wake → 復元 → ACK の往復処理はコアプロセス内で完結する。他 GUI 所有を理由とする NACK や調停待ち処理は不要となる。

### 8. 更新・再起動: 安全点再起動と自動再開

コアプロセスのバイナリ更新や設定変更を円滑に行うため、「安全点再起動＋自動再開」機構を設ける。

- **安全点での停止手順**:
  1. コアは新規ターンの開始を停止する。
  2. 実行中のターンについて、tool call 境界（モデルへのリクエスト前または tool 完了後）まで進行させて checkpoint を保存する。
  3. コアプロセスを安全に終了する。
- **再開意図の永続化と自動再開**:
  - 停止直前に実行中（Active）だった run の再開意図をストレージに永続化する。
  - 新しいコアプロセスの起動時に、永続化された再開意図を読み込んで自動的に run を復元・再開する。
- **長時間シェルジョブの扱い**:
  - コア再起動時に実行中のバックグラウンド shell job が存在する場合、再起動画面に一覧表示する。
  - ユーザーは「ジョブの完了を待ってから再起動」または「ジョブを中断して再起動後にモデルへ再実行を任せる」を選択できる。既定値は「待つ」とする。
- **DB スキーマ更新**: データベーススキーマの変更を伴う更新の際も、停止・マイグレーション・再起動の対象が単一のコアプロセスのみとなり、複数プロセス間の競合を排除できる。

### 9. 障害隔離と再接続

- **Panic 隔離**: run 単位の panic 隔離（既存の `AgentRunPanicked` イベントおよび `catch_unwind`）はコア内でそのまま維持する。1 つの run の失敗がコアプロセス全体を巻き込まないようにする。
- **再接続と自動リカバリ**: コアプロセスとの接続が切断された場合、デスクトップ GUI クライアントはコアプロセスの自動再起動と再接続を試みる。
  - リトライ上限: 短時間に 3 回までの再起動・再接続を試行する。
  - 上限超過時: 連続して失敗した場合は再起動を停止し、ユーザーにエラーを表示して手動復旧を促す。
- **残存リスクの明記**: プロセス単位の致命的障害（メモリ不足による OOM Killer、外部からの `kill -9`、プロセス abort 等）が発生した場合は、そのコアが扱っていた全プロジェクト・全作業が同時に影響を受ける。これは単一コア化に伴う残存リスクとして受容し、安全点 checkpoint とストレージ復旧契約（[ADR 0027](0027-restore-contract.md)）により復旧可能性を担保する。

### 10. Headless `evorch run` の動作

CLI コマンド（`evorch run` 等）は以下のハイブリッド構成とする。

- 同一設定ディレクトリのコアプロセスが既に稼働している場合は、クライアントとしてコアに接続してコマンドを発行する。
- コアプロセスが稼働していない場合は、単体で動作する。

### 11. アーキテクチャ境界の CI 検証

内部 crate 間の依存方向を確実に保つため、Cargo metadata（`cargo metadata`）ベースの静的依存検査を CI に組み込む。

- `crates/gui` や client crate が `crates/runtime` や `crates/storage` などのコア内部実装 crate に直接依存することを禁止する。
- 移行期間中は、既存の依存違反を allowlist として固定し、移行ステップの進捗に合わせて allowlist を段階的に削減・全廃する。

### 12. 段階的移行順序

並行作業との競合を避け、安全に移行を進めるため、以下の順序で実装する。

1. **Protocol / Client 層の策定**: `crates/protocol`（Command/Query/Snapshot/UiEvent/DTO）およびクライアント通信基盤の定義。
2. **Host ロジックの実装**: コア側のコマンド・クエリハンドラ、および承認要求の有効性管理（有効期限や解決済み判定）の実装。
3. **Web Adapter とスマホ向けフロントの構築**: HTTP + SSE の Web サーバー機能と、スマホ操作用 Web フロントエンドの実装。
4. **コア独立バイナリの構築と接続**: `evorch-core` バイナリの分離、単一インスタンス管理、およびプロセス間接続の実装。
5. **安全点再起動＋自動再開の実装**: checkpoint 保存、再開意図の永続化、および自動復元パイプラインの構築。
6. **デスクトップ GUI のクライアント化**: デスクトップ GUI の接続先をコアプロセスへ切り替える。この時点でデフォルトの動作モードを単一コア構成へ切り替える（安全点再起動機構が安定する前には切り替えない）。
7. **旧所有権機構・共有 DB 同期の撤去**: `owners.db`、heartbeat/claim、`readonly_threads`、`shared_feed`、`sidebar.json` merge を完全削除。

**並行開発との競合回避**:
現在並行して進んでいる PR #473（GUI 本文遅延読込）、PR #477（lane runtime）、および ADR 0044 Phase 1c と競合する箇所（`crates/gui/src/bin/evorch-gui.rs`、`crates/gui/src/app/history.rs`、`crates/gui/src/body/`、および Phase 1c の GUI wake 経路）の改修は、これらの PR および Phase 1c の完了を待ってから着手する。

## Consequences

### メリット（消せる複雑さ）
- [ADR 0024](0024-process-owner-claim.md) に由来する `owners.db`、Unix domain socket による claim/handoff、heartbeat、`Suspect`/`Stale`/`Claimable` 状態遷移、generation fence のコードを全廃できる。
- `owners.db` のロック競合による run のエラーや、終端イベント欠落による GUI のゾンビ実行中表示が根本的に解消される。
- 各 GUI が SQLite を直接ポーリングして同期していた `shared_feed` や、`sidebar.json` の 3-way merge の複雑さを撤去できる。
- 外出先・別端末（スマホ等）から安全に会話の閲覧、返答、質問への回答、ツール承認を行えるようになる。
- GUI プロセスを気軽に閉じたり再起動したりできるようになり、GPU/描画の不具合が実行中エージェントに波及しなくなる。

### デメリット・新たに必要な仕組み
- **単一インスタンス管理**: ロックファイルとソケットの健全性判定、プロセス起動・終了のライフサイクル管理が必要になる。
- **バージョン不一致のハンドリング**: コアとクライアントのバージョンが異なる場合、稼働中のジョブを勝手に強制終了させず、GUI 側の再起動やコアの再起動を促すプロトコルネゴシエーションが必要になる。
- **切断と再接続**: ネットワーク瞬断やコア再起動時のクライアント再接続処理、および未配信イベントの再同期（Last-Event-ID / snapshot）が必要になる。
- **承認要求の有効性管理**: 過去の `ApprovalRequested` イベントがクライアントの再接続やリロードで replay された際に、既に解決済みの承認要求を復活させないよう、コア側で承認要求の有効状態を一元管理する必要がある。
- **障害範囲の拡大と緩和**: 単一コアプロセスのクラッシュ（OOM 等）が全作業に波及する。これに対し、run 単位の catch_unwind 隔離、自動再起動、および安全点 checkpoint による復旧の確実化が不可欠となる。

## Alternatives

- **現状維持（GUI ごと runtime 内蔵＋ADR 0024 所有権モデル）**:
  複数プロセスのロック競合や表示の不整合が残り続け、Web/スマホ対応のたびに別プロセスとして所有権調停へ参加させる必要があり、保守コストが破綻する。
- **常時常駐 daemon（systemd 等によるサービス化）**:
  evorch を起動していない時もバックグラウンドでリソースを消費し続ける。また OS ごとのサービス登録や権限管理の負荷が増える。オンデマンド自動起動の方が利用実態に合致する。
- **最初の GUI プロセス内にコアを置き、他 GUI が接続する方式**:
  最初に起動した GUI ウィンドウをユーザーが閉じると全停止してしまう。また GUI 側の描画不具合やクラッシュにコアが巻き込まれる。
- **プロジェクトごとのコアプロセス分離**:
  evorch では同一の SQLite データベースやストレージをプロジェクト間で共有しているため、DB スキーマ変更時の全体停止問題が解消されず、所有権調停の複雑さも残る。
- **eframe wasm / 画面ストリーミング（VNC や WebRTC 等）による Web 操作**:
  スマホ環境における IME 日本語入力、テキスト選択、アクセシビリティ（a11y）、画面解像度追従に著しい難がある。さらに現行 GUI が 16 crate に直接依存しており、そのまま wasm 化することは極めて困難である。

## 未決事項

- **Unix domain socket 上の Desktop 用 transport の詳細**:
  JSON-RPC 2.0、長さプレフィックス付き serde_json、あるいは WebSocket over Unix socket のいずれを採用するか。
- **コアのアイドル終了の猶予時間**:
  最後のクライアントが切断されてから、何分間待機してコアプロセスを安全に終了させるか（例: 5 分〜15 分）。
- **tool call 境界で止められない tool の扱い**:
  外部 LLM provider からの長文ストリーミング受信中や、外部 API コール待機中の turn をどのように安全点として中断・再開するか。
- **複雑な状態での再起動復元確認**:
  `ask_user` の未回答待ち状態、delegate された子 run の実行中状態、および [ADR 0042](0042-event-driven-waits.md) の wait 状態における、再起動後の復元動作の網羅的検証。
- **クロスプラットフォーム対応**:
  現時点では Linux first（[ADR 0009](0009-platform-linux-first-gui-only.md)）であるが、将来の Windows / macOS 対応における IPC（Named Pipes 等）の抽象化設計。
- **認証の後日導入**:
  Tailscale 外部からのアクセスを許可する場合の、セッショントークン発行やパスワード認証の仕様。

## Related

- [mission.md](../../mission.md): 原則 6（GUI は Headless Agent Kernel の上に乗るだけ）。
- [ADR 0003](0003-cache-first-context-engine.md): キャッシュ第一コンテキストエンジン（run 内安定性）。
- [ADR 0005](0005-headless-kernel-and-gui-separation.md): Headless Agent Kernel と GUI の分離。
- [ADR 0009](0009-platform-linux-first-gui-only.md): プラットフォーム方針（Linux first GUI）。
- [ADR 0017](0017-event-bus-transport.md): Event Bus の transport（in-process tokio broadcast のコア内維持）。
- [ADR 0024](0024-process-owner-claim.md): thread process owner / claim モデル（本 ADR により置換・撤去予定）。
- [ADR 0027](0027-restore-contract.md): 復旧契約（restore contract）。
- [ADR 0037](0037-owner-checkpoint-metadata.md): owner checkpoint metadata。
- [ADR 0042](0042-event-driven-waits.md): イベント駆動待機。
- [ADR 0044](0044-lane-threads.md): lane を独立した Orchestrator thread として管理する。
