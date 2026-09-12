# ADR 0025: エージェント環境の体験評価に基づく機能方向（2026-09-12）

## Status

Accepted

## Context

2026-09-12 の 11 機能改善 wave（18 タスク並列、Oracle レビュー 3 回、セッション障害 4 回、
コンテキスト圧縮 2 回を経て完走）を、opencode + oh-my-openagent 環境で orchestrate した
エージェント（Sisyphus / kimi-k3）自身の体験記録と、それに基づく evorch への機能評価。

評価の出発点はユーザーからの依頼: 「作業している環境と作っている環境を比較して、
evorch にあった方が良い機能・強化すべきもの・入れない方が良いものを体験をもとに評価してほしい」。
ダッシュボード/情報表示の話はユーザー側の体験の問題として本 ADR の対象外とし、
エージェントの作業体験に効く項目のみを記録する。

## Evidence（体験の事実）

- サブエージェントの session 継続（task_id によるフルコンテキスト再開）が T13 の 3 度の
  stream 断を救った。逆に session 失効（Task not found）で T10 の文脈が全損し、
  新規 session への立て直しコストが発生した。
- 並列エージェント間の共有 git index 競合で混在コミットが 2 度発生（86f2ad3, 00d6055）。
  内容は正しいが atomic でなく、履歴の信頼性を静かに削る。
- `/tmp` の notepad（圧縮されない durable な追記メモ）+ compress の組み合わせが
  長時間作業の生存戦略として有効だった。
- 完了通知駆動（fire して turn を終え、system-reminder で起きる）はポーリングより
  トークン効率・応答性ともに優れていた。最大 5 タスクの並列 background を捌けた。
- Oracle（read-only 高 IQ レビュー）が T9 の confused-deputy 承認脆弱性を発見し
  needs-rework 判定 → rework で修正。並行制御・セキュリティ領域での費用対効果が高い。
- provider の 'stream disconnected' が同一タスクで 3 連発。無条件リトライは無効で、
  3 連敗で停止して経路を変える（新規 session）のが有効だった。

## Decision

### 採用（packet 化済み — v09 シリーズ）

1. **v09-a: send 統合の session 継続 + run 台帳**
   run をフルコンテキスト保持で再開するプリミティブと、compaction を跨いでも失われない
   append-only の作業台帳。最優先（体験上の効果が最大）。
   2026-09-12 の協議で確定: 別プリミティブではなく **send ツールの統合 interface として**
   実装する（生存中=現行 mailbox 配送、終了/不在=storage projection から復元して新規 turn
   として注入。Done/Error の再開 semantics は「新しい turn として注入」に統一。
   復元は重いため内部機構は分割し、復元時に event を発行して観測可能にする）。
2. **v09-b: 完了通知駆動のバックグラウンド run モデル**
   ポーリングではなく event 通知で起きるモデル。F8 の ack モデルを再利用。
3. **v09-c: ストリーム障害の回復設計**
   bounded retry（指数 backoff、既定 3 回）+ 連敗時 escalation。T10/T17 の
   部分表示契約を壊さない。認証エラーは retry しない。
4. **v09-d: 承認 UX の相関 ID 明示**
   d5342ce の runtime 修正（run:call:attempt 束縛）の GUI 側仕上げ。

### 既存 intent の優先度を裏付け（新規 packet 不要）

5. **v02-workspace-isolation（runtime 所有 git worktree 隔離）**: 今回の index 競合は
   この unit が解決する問題の実証。worktree 隔離が先にあれば混在コミットは起きない。
6. **v02-context-compaction（監査可能な compaction）**: raw history 非破壊・発動 event の
   観測可能性という設計方向は正しい。体験上の要求と一致。

### 不採用（入れない方が良いもの）

1. **可視性のない自動圧縮（silent compaction）**: 何を捨てたか見えない圧縮は事故る。
   圧縮しても外部（storage / CLI / ファイル）から再構成できる設計のみ信用する。
2. **隔離なしの大並列**: 並列度は隔離機構（worktree 等）のキャパシティを超えない範囲に
   制限する。宣伝文句としての並列数拡大は品質を静かに削る。
3. **不可逆操作の自動判定による実行**: push / 送信 / 削除 / ラベル変更は
   explicit request ゲートを残す。F7 の自動 write-mode が機能したのは
   「未所有→claim、他人所有→通知のみ」とスコープを絞ったからであり、
   この抑制を一般化しない。
4. **情報の全部乗せ表示**: 価格不明時・cache<10% 時の非表示ルールのような抑制を
   仕様として維持する（本 ADR では方針の記録に留め、具体設計はユーザー体験側で扱う）。

## Consequences

- v09-a〜d が queue に積まれ、v02-workspace-isolation / v02-context-compaction の
  優先度根拠が強化された。
- エージェント体験に基づく評価を定期入力として使う先例ができた。
  次回の同種評価は improve / inspect のインプットとして扱える。
