# 簡潔な agent 指示の調査

調査日: 2026-10-15。実装の事実、著者の測定結果、evorch への提案を区別する。
結論は、モデルが判断できる一般論を減らし、必要な契約を必要な相手に渡す方針を支持する。
文字数だけで性能が決まる、すべてのモデルで同じ削減が安全、という根拠にはならない。

## Cursor と Claude の報告

[Cursor: Improving token efficiency（2026-09-23）](https://cursor.com/blog/improved-token-efficiency) は、モデル別の system prompt を約66%削減し、禁止事項の列挙よりツールの振る舞いを明確にする方針を報告している。低頻度のツール定義は必要時に取得し、よく使うツールは残す。実タスクでの A/B 評価、安定した cache prefix、ツール出力の調整も含む施策全体で、token cost は7%減ったという。66%は prompt の削減率であり、費用の削減率ではない。文章の削減だけの効果も分離されていない。

[Claude: The new rules of context engineering for Claude 5-generation models（2026-07-24）](https://claude.dev/blog/the-new-rules-of-context-engineering-for-claude-5-generation-models/) は、Opus 5 / Fable 5 向け system prompt の80%以上を除いても coding eval で測定可能な低下がなかったと報告する。重複・競合する指示や一般的な作業手順を減らし、明確な interface、必要時に読む skill、タスク固有の情報を重視する。詳細な仕様まで削る勧めではなく、対象モデル・評価集合に依存する結果である。

evorch では、ツールの非自明な作用はツール説明へ、ロールの責務はそのロールの指示へ置く。一般的な問題の解き方や、実装で自動的に満たされる手順を常時説明する必要は薄い。一方、親の委譲判断と子の応答には別々に必要な情報がある。同じ事実が別の受け手に届くことを、単純な重複とは扱わない。

## pi: 小さな標準構成と拡張の分離

確認した [pi](https://github.com/earendil-works/pi) の revision は `a3f265b1c6a49bc0904c76518d2d436ba44c95e9`。

- [標準の coding tools](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/src/core/tools/index.ts#L164) は read / bash / edit / write の4つ。grep / find / ls などの実装もあり、利用可能な能力全体が4つという意味ではない。
- [system prompt の組み立て](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/src/core/system-prompt.ts#L88) は、選択ツールに応じた guideline を追加し、同じ guideline を重ねない。短い役割説明に、ツールの用途、project context、skill の一覧、cwd、追加設定を組み合わせる。[現行の本文](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/src/core/system-prompt.ts#L150) には pi 自体を扱う際の資料案内もあり、常に一文だけという設計ではない。
- 契約は具体的に残す。[edit](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/src/core/tools/edit.ts#L43) は複数編集の照合対象や重複の扱い、[read](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/src/core/tools/read.ts#L94) は出力上限・続きの読み方、[write](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/src/core/tools/write.ts#L50) は作成・上書きと親ディレクトリ作成を説明する。
- [skills](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/docs/skills.md) は名前・説明・参照先を先に渡し、本文は必要時に読む。選択が常に成功する保証はなく、明示的な skill 指定も備える。
- 現行版は [MCP をサポート](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/docs/mcp.md#L189) する。codemode はスクリプト内の探索、deferred は tool_search、direct は常時公開、hidden は非公開という選択がある。サーバー要約の変更は会話へ追記する設計で、単純に毎ターン宣言を入れ替えていない。
- [agent loop](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/agent/src/agent-loop.ts#L179) や [compaction](https://github.com/earendil-works/pi/blob/a3f265b1c6a49bc0904c76518d2d436ba44c95e9/packages/coding-agent/docs/compaction.md) は実装として存在する。小さな指示文と、継続・中断・履歴管理をコードで扱うことは両立する。

[作者の2025-11-30の記事](https://mariozechner.at/posts/2025-11-30-pi-coding-agent/) の「system と tools を合わせて1000 tokens 未満」や MCP を持たないという記述は、当時の構成についての説明である。現行版の token 予算や能力として転載しない。同記事の Terminal-Bench 2.0 比較は、pi + Opus 4.5 などの組み合わせによる著者の評価で、文章量だけを変えた実験ではない。[再現用 adapter](https://github.com/badlogic/pi-terminal-bench) と [実行結果](https://gist.github.com/badlogic/f45e8f6e481e5ab7d3a50659da84edaa) は公開されているが、全モデルでの優位性は示さない。

evorch に適用しやすいのは、標準経路を少数の明確な interface にし、条件付き機能を発見可能にすること、実装固有の契約を短く残すことである。pi の権限構成や、標準で持たない機能まで一括して採用する理由にはならない。

## DeepSeek Harness: Minimal と Standard は能力構成も違う

確認した [DeepSeek Harness（DSH）](https://github.com/deepseek-ai/deepseek-harness) の revision は `86a402200d95fecef4529d969836f1fe13c6cb0d`。plugin を合成するハーネスであり、プロジェクト全体が小さな単一ループだけで構成されるわけではない。

| 観点 | 確認できる実装 |
| --- | --- |
| Minimal | [preset](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/bundle/web-app/presets/minimal.patch.yml#L14) は短い persona を complete system prompt とし、optional runtime context を抑制する。独立した read / write / edit は無効にし、画像読み取り・永続 shell・compaction を組み合わせる。shell は書き込みも可能なので、read-only の構成ではない。 |
| 説明が置かれる場所 | 同じ [Minimal の shell 定義](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/bundle/web-app/presets/minimal.patch.yml#L52) に、状態の永続性・出力・長時間コマンドなどの説明を残す。system が短くても、ツール説明を含む入力全体は一文にはならない。 |
| Standard | [preset](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/bundle/web-app/presets/standard.patch.yml#L15) は project instructions、filesystem/search、jobs、skills、goal、subagents、web、todo などを追加する。plan mode の詳細な制約は条件付きで、cache のため mode 間でも tool catalog は維持する。 |
| PTC | [agent-tool-presentation](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/core/agent-tool-presentation/README.md) は native schema と run_code + generated SDK の提示を切り替える。多数の直接呼び出しをまとめる方式であり、説明の費用がゼロになるわけではない。 |
| 遅延読み込み | [skills](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/docs/subsystems/skills.md#L227) は一覧と本文を分離し、skill 呼び出しで本文、必要時に resource を取得する。過去の結果を本文更新で書き換えない。 |
| 制御と cache | [tools](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/core/tools/README.md) の型検証・実行 policy と、[prompt assembly](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/core/system-prompt/README.md#L140) の context・schema は別の仕組み。短い persona だけに制御を担わせていない。 |
| 反復への対処 | [repeat-tool-reminder](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/guard/repeat-tool-reminder/README.md#L74) は同一 tool + canonical args に対して閾値で注意を追記する advisory で、実行の veto ではない。引数を変えて続く今回の planner 型の停滞を一般に検出する保証はない。 |

[system prompt の cache 説明](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/core/system-prompt/README.md#L150) は、route が対応する in-history update と、先頭の system node を置換する場合を区別している。後者は変更位置から prefix reuse を失う。DSH の動的機能を evorch へ移す際も、provider の対応と既存 prefix を維持できる境界を確認する必要がある。

### 同じモデルでの公式 scaffold 比較

[DeepSeek-V4.1-Flash の公式 model card](https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash#performance-across-agent-scaffolds-deepswe-v11-and-terminal-bench-21-max-reasoning-effort) は、最大 reasoning effort の同一モデルで比較を公開している。

| Scaffold | DeepSWE v1.1 Resolved | Terminal-Bench 2.1 Pass@1 |
| --- | ---: | ---: |
| Claude Code | 69.8 | 88.0 |
| Codex | 65.6 | 84.1 |
| OpenCode | 65.5 | 85.0 |
| Pi | 66.2 | 86.1 |
| mini-SWE | 74.2 | 90.3 |
| DSH Minimal | 72.6 | 90.6 |
| DSH Standard | 70.5 | 85.8 |
| DSH PTC | 67.6 | 85.8 |

公開条件は Linux container、1M context、max_steps=500、temperature=1.0、top_p=0.95。DeepSWE は task ごと8 samples、Terminal-Bench は3 samplesで、後者は network 無効。
この条件では Minimal が Standard を上回り、mini-SWE も強い。pi が常に最良ではない。「能力を絞った構成でも高性能」という根拠にはなるが、prompt の長さだけを操作した比較ではなく、全モデル・全タスクでの順位や費用を証明しない。

### コミュニティの比較を読む際の注意

- [explicit-edit-benchmark](https://github.com/alexshpunt/explicit-edit-benchmark) は、226の決定的な編集タスクと byte-exact verifier、pi / DSH などの adapter、設定・実行結果を公開する。狭い契約を揃えた比較の参考になるが、曖昧な要求・調査・長期の debugging を代表するとは限らない。
- [DSH discussion #9160](https://github.com/deepseek-ai/deepseek-harness/discussions/9160) は同じ端末・endpoint・model で startup 入力と cache usage を測り、他の投稿と異なる費用傾向を示す。task 比較は小さな2件で、権限設定も異なると著者自身が明記する。HTTP request 全体の bytes と model が読む入力を区別する点は参考になるが、恒常的な費用優位の証拠にはしない。

## evorch での比較方法

まず、能力と schema を変えずに一般論・同じ受け手への重複だけを減らす。次に、条件付き説明の移動やツール提示方式を別の変更として評価する。こうすると、短文化と能力構成の効果を混同しにくい。

代表ケースには、通常の実装、read-only 調査、planner への不可能な書き込み要求、レビュー、ユーザーの途中訂正を含める。主モデルと fallback モデルで成功・不要な呼び出し・未完了・制約の誤報告を確認し、実際の provider 入力と usage から新規入力・cache read・出力・呼び出し数を測る。弱いモデルでも改善するかは、この評価で判断する。

文字数、token 数、費用、成功率は別の指標である。短くした結果、説明を取りに行く呼び出しや再試行が増える場合も含めて比較する。外部資料の結果を evorch の改善率として扱わない。evorch での適用・検証結果は [監査資料](evorch-audit.md) に記録する。
