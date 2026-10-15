# evorch の agent 指示・ツール説明の最適化候補

調査日: 2026-10-15。対象 revision: `198d46a3170c71ad7bb9d183dcaabd358c87bda3`。
以下は実装と指示文の監査による仮説であり、短文化による性能改善はまだ測定していない。
候補の修正はこの skill 追加の範囲に含めない。適用時は現在の実装を読み直す。

## どの指示が届くか

[assembly.rs:69](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/prompt/assembly.rs#L69) は role baseline → model family → optional category → Orchestrator 専用 Intent Gate / keyTriggers → appendix の順に合成する。
[catalog.rs:122](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/prompt/catalog.rs#L122) で選ぶ family は1つであり、全 family が毎回入るわけではない。category の appendix は role の appendix より優先するが、baseline・family・category は加算される。

skill 本文は既に [skill_load](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/meta/specs.rs#L123) で必要時に読む構成である。権限は [policy.rs](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/policy.rs#L116) などの実行側でも扱う。指示文の削減と権限チェックの削除を一緒に行わない。

## 先に検証したい候補

### 1. family 指示の受け手を正す

対象: `crates/config/assets/presets/family-*.md`。例: [generic:5](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/family-generic.md#L5)、[kimi:5](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/family-kimi.md#L5)。

「役割・完了条件を冒頭に提示する」「出力例を示す」「参照資料を先に、指示を後に置く」など、prompt を作る人への助言が実行する agent に渡っている。agent が受け取った system prompt を並べ直すことはできず、ユーザーへの回答に例や指示を増やす解釈もあり得る。

author 向け助言は runtime から外し、モデル固有の振る舞いに効果を示した差分だけを残す候補。family resolver の API や未知の family の扱いを変える必要はない。比較では回答への余計な指示・例の混入、task 成功、各モデルでの劣化を確認する。

### 2. Orchestrator の委譲指示を責務と成果へ絞る

対象: [role-orchestrator.md:10](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/role-orchestrator.md#L10)。

直接対応の例外、調査の委譲、役割選択が複数箇所にあり、delegate の説明とも重なる。固定の5見出し TASK / EXPECTED OUTCOME / MUST DO / MUST NOT DO / CONTEXT は、必要な情報を渡す目的に比べ形式を強く規定している。

Orchestrator 自身の優先順位・依存・委譲・受け入れ判断と、子へ渡す目的・担当範囲・制約・完了条件を残す。役割ごとの能力一覧は delegate 側へ寄せる。ユーザーや AGENTS.md の委譲方針は尊重する。比較では担当衝突、情報不足、過剰な細分化、受け入れ漏れを確認する。

### 3. meta tool の schema と本文の重複を整理する

対象: [specs.rs:17](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/meta/specs.rs#L17) の delegate と、[specs.rs:59](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/meta/specs.rs#L59) の wait、および周辺の message / inbox tools。

wait の description は1937文字の ASCII で、ID の排他、件数、enum、timeout の上限・既定値など schema にある情報も繰り返す。delegate は全体説明と target.role の branch に planner の能力を重ねる。同じ親が両方読むため、先日の修正で明確にした制約を保ちながら整理できる。

引数の規則は schema の適切な箇所へ寄せる。ただし provider が branch の description を実際に渡すことを確認してから本文を減らす。親への能力説明と、子の [planner 指示](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/role-planner.md#L13) は別の判断に必要なので両方を残す。

wait の attention / user input、inbox を消費しないこと、Waiting が終端ではないこと、timeout が cancel ではないこと、対象 run の関係制約は保持する。message と inbox の消費・復元の差も契約である。結果の細部を別資料へ移す場合は、誤読を防げる discovery が必要。比較では完了・質問・ユーザー割り込み・timeout・cancel と、メッセージの取りこぼしを確認する。

### 4. ロールと category の固定手順・出力の重なりを減らす

対象: [planner:5](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/role-planner.md#L5)、[worker](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/role-worker.md)、[quick](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/category-quick.md)、[deep](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/category-deep.md)。

planner の計画項目と出力見出し、worker baseline と category の検証・報告が重なる。quick のテスト回数や報告行数、deep の検証順序はタスクに合わない場合もある。

必要な成果・実施した検証・未解決事項を指定し、検証の深さは変更の影響に応じて選ばせる候補。planner の read-only 境界、不可能な要求への説明、計画の実施を親に委ねる点は保持する。失敗したテストを回数制限で放置したり、成功していない検証を成功と報告したりしないかを確認する。

### 5. conversation に常時入る TODO / Goal 操作の詳細を条件付きにする

対象: [category-conversation.md:11](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/config/assets/presets/category-conversation.md#L11)。

会話の category に状態管理の細かな手順が常時含まれる。操作の意味は該当ツールの説明へ、長い条件付き手順は必要時に読む資料へ移す候補。いつ TODO / Goal を使うかの判断材料、pause / stop のユーザー意図、依頼の範囲を拡張しない条件は残す。短い会話での不要な状態操作と、長い依頼での追跡漏れの両方を評価する。

## 影響範囲を調べてから判断する候補

| 対象 | 観察と次の確認 |
| --- | --- |
| [Intent Gate policy](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/prompt/intent_gate_policy.rs#L90) と [entry routing](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/entry_routing/reclassify.rs#L109) | 分類軸と判断を別の段階で扱う。router が検証済みの判断を引き渡せるかを調べる設計課題。新規 user message、現在の依頼、mutation の許可範囲を失う単純な削除は避ける。 |
| [keyTriggers](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/prompt/key_triggers.rs#L52) | role 一覧と skill metadata は discovery を担う。delegate と重なる説明は候補だが、skill 本文は既に遅延読み込み。動的な一覧削減は選択漏れと cache を評価してから行う。 |
| [compaction context](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/base_context.rs#L206) と [policy](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/crates/runtime/src/compaction/policy.rs#L258) | runtime が強制する threshold / cooldown の説明を、agent が判断に使うか確認する。未完了の目的・決定・検証状況を残す要約契約と、履歴置換の境界は維持する。 |
| explorer / reviewer / web-researcher / oracle の根拠・不確実性の指示 | 受け手が異なるため、横断的に一箇所へ移すだけでは共有にならない。各ロール内の重複を優先し、ファイル・行、一次資料 URL、review severity など成果に必要な契約を残す。 |
| 権限・出所・trusted context の指示 | 長いことだけを削除理由にしない。実行側の強制、判断に必要な境界、攻撃・誤帰属に対する説明を分けて評価する。 |

## 静的な量の目安

調査 revision の preset 本文を Unicode 文字数と UTF-8 bytes で数えた値。token 数、provider 入力全体、費用の測定ではない。

| Preset | 文字数 | UTF-8 bytes |
| --- | ---: | ---: |
| family（選択される1つ） | 153–174 | 386–436 |
| role-orchestrator | 1328 | 3476 |
| role-planner | 753 | 1991 |
| role-worker | 232 | 596 |
| category-conversation | 937 | 2343 |
| category-quick | 146 | 364 |
| category-deep | 158 | 416 |

複数 family や無関係な category の量を、毎回の削減余地として合算しない。preset 以外に生成される指示や tool schema もあるため、実際の provider request で内訳を確認する。

## 検証時の境界

本文の整理、説明の条件付き取得、tool catalog の変更は別々に評価する。通常ターン中の system / tools / 送信済み prefix の改変で削減しない。cache に関わる実装変更は [check-cache-contracts.sh](https://github.com/turtton/evorch/blob/198d46a3170c71ad7bb9d183dcaabd358c87bda3/scripts/check-cache-contracts.sh) の契約と、変更経路の独立した prefix 検証を保つ。

今回の planner 事例の再現を含め、代表的な task の成功率・不要な tool call・未完了・誤った制約報告・総 token / cache を比較する。プロンプト文字列の一致テストや固定された最終回答だけでは、モデルが指示を理解した証拠にならない。
