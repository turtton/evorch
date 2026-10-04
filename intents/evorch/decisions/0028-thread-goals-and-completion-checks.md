# ADR 0028: 汎用thread goalと完了チェック

## Status

Accepted（2026-10-04）。利用者との設計相談で合意し、実装・PR・マージまで依頼された。

## Context

既存のgoal supervisorはPR作成・CI・Reviewer承認・merge・closeoutを前提とする。
一方、[ADR 0001](0001-no-fixed-workflow.md) は仕事の種類に応じた動的な実行を求めている。
調査、設計、文書作成、PRを作らないローカル修正にも、依頼を完遂するためのgoalが必要である。
通常会話のrunはターン終了後も入力待ちとして存続するため、run終了だけを継続の契機にできない。

関連feature: [Orchestration](../features/orchestration/overview.md)、[GUI Workbench](../features/gui-workbench/overview.md)、[Agent Runtime Kernel](../features/agent-runtime-kernel/overview.md)。

## Decision

### Goalの作成と範囲

- 会話のroot agentは、依頼の完遂に必要だと判断したときにgoalを設定できる。利用者の明示的な`/goal`投入も同じ汎用goalを使う。
- Goalはそのthreadで完遂したい仕事を表す。PR、commit、CI、mergeを一律の完了条件にしない。必要性は元の依頼に従う。
- 元の依頼と完了条件を記録し、agentがgoal化を理由に依頼範囲や操作権限を広げない。子agentが別threadのgoalを設定・完了させることはできない。
- 1 threadの未完了goalは1つ。完了したgoalは次のgoalまで確認可能にする。会話途中のgoal設定でrunを作り直さず、履歴と消費済み予算を維持する。

### 完了確認

- 自然なターン終了と明示的な`finish`の両方で、runtimeが未完了goalの自己確認を促す。agentは元の依頼、各完了条件、成果・検証の証拠を照合する。
- 未達なら同じ作業agentが継続し、達成を確認できた場合だけ完了とする。質問への回答待ちや外部の障害を無意味な反復で解消しようとしない。
- 独立reviewはgoalごとの任意設定で、既定は無効。利用者がUIから有効化した場合、自己確認後に別contextのread-only reviewerが依頼と実際の成果を確認する。
- Reviewで未達・不具合・証拠不足が見つかった場合、元の作業agentが修正・自己確認・再reviewを行う。依頼に不要な改善提案だけで完了を妨げない。
- 自己確認とreviewは累積予算・反復上限の内側で実行する。上限や確認不能は未完了理由として表示し、成功扱いにしない。
- 上限到達でBlockedになったgoalは、利用者が明示的に`/goal <objective>`を送った場合だけ新しいgoalと予算で置き換えられる。agentの`create_goal`や通常の継続・Resumeでは上限をリセットしない。進行中・Pause中のgoalの無断置換も認めない。
- 停止、Pause、追加依頼、成果変更によって古くなった判定は完了や自動再開の根拠にしない。

### Stopとchecks Pause

- ChatboxのStopは作業を停止し、自動継続も抑止する。既存の子agent停止の操作を維持する。
- GoalのPauseはターン終了時の反復・自己確認・独立reviewを一時的に無効化する。実行中の作業agentは止めない。進行中のreviewerは中止し、遅延結果を無効化する。
- Checks Pauseは明示的なResumeまで保持する。追加メッセージ、作業の再開、アプリ再起動で勝手に解除しない。
- Checks Resumeは最新の依頼・成果に対して再開する。作業中なら次の終了境界で、入力待ちなら重複起動なしに確認を始める。作業Stop後は作業再開の操作も必要とする。

### Conversation UI

- Chatbox直上に、chatboxにつながる一段狭い丸角のgoal表示を置く。
- 通常は1行にgoal本文、review切替、checks Pause/Resume、詳細展開を並べる。ボタンはアイコンとtooltip・アクセシブル名を持つ。
- 「作業中」という常時ラベルや、chatboxと重複するStopボタンは設けない。長い本文は省略し、詳細から全文を読める。
- Review有効は選択状態で表し、review合格済みの表示とは区別する。Pause、確認中、Blocked、完了の状態を必要に応じて判別できるようにする。
- 詳細には元の依頼、完了条件、証拠、review指摘、未完了理由を表示する。別threadのgoalを混ぜない。

### 永続化と既存機構

- Goal状態、明示Pause、review設定、条件・証拠・指摘、反復・予算をイベントから復元する。保存内容だけで旧runや旧権限を自動復活させない（[ADR 0027](0027-restore-contract.md)）。
- 既存のWorkerからOrchestratorへの引き継ぎでは、goal・Pause・累積予算・現在のownership・会話の継続性を保持する。旧root配下に残った子agentも、使用量と完了待ちの対象から外さない。
- 汎用goalを旧PR delivery supervisorと分離する。旧PR経路のmerge承認・closeoutや過去のpacket/interviewは履歴として保持する。
- Goal toolのschemaはrun開始時から安定させる。自己確認・修正指示は履歴末尾に追加し、送信済みprefixやsystem/toolsをgoal状態に応じて書き換えない。

## Consequences

仕事の種類を固定せず、完了条件を満たすまでの継続を支援できる。Reviewには追加の費用と待ち時間があるため、常時強制せず利用者が選択する。作業の停止状態と完了チェックの停止状態を別々に管理する必要がある。

## Validation

PRなしの完了、自然終了とfinishの確認、review差し戻し後の完了、Pause中の作業継続、Stop/Pause/追加依頼後の遅延review拒否、再起動・複数threadの分離、累積予算、provider wireのprefix保持を契約テストで確認する。
