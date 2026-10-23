# ADR 0035: Oracle を相談専用 Advisor に改称する

Status: Accepted → 2026-10-23 に [ADR 0040](0040-advisor-stuck-point-triggers.md) で、Advisor を起用する 3 つの場面（再レビュー後も残る指摘・Worker と Reviewer の食い違い、仕様では決まらない技術判断、絞りきれない原因仮説）を定めた。

## Context

ADR 0002 のロール境界を維持しつつ、判断前の相談と独立した受入レビューを明確に分ける。
Oracle がレビューに使われた過去の記録（ADR 0025 など）は歴史として残す。

## Decision

- Oracle は新ロールを追加せず Advisor に改称する。read-only、delegate 不可の権限は不変。
- Advisor は必要時のみ、判断前の設計案・原因仮説を比較して助言する。固定工程ではない。
- 出力契約は「選択肢 / トレードオフ / 推奨案 / 不確実性 / 判断を変える追加情報」。
- 実装は Worker、実装結果・計画の仕様照合と受入判定（合否）は Reviewer、詳細実行計画は Planner が担当する。
- 相談した Advisor と最終独立 Reviewer は別 context にする。これは親への運用案内であり、新たな runtime 権限や工程強制ではない。

## Compatibility

大きな互換レジストリや新しい config version は導入しない。
既存 model-chain migration と同じく、ファイル単位・マージ前・version に依存しない正規化を行う。
`agents.roles.oracle` と role profile 内の同 binding を `advisor` へ移す。
環境変数・CLI・設定保存時のプレビュー層もマージ前に正規化する。
同じ層に両名があれば既存 deep-merge を用い、Advisor のフィールドを優先する（警告あり）。
層をまたぐ場合は従来どおり上位層が優先する。未知フィールド・カテゴリは従来どおり strict validation で拒否する。
直接 serde で読む場合も旧 binding を alias として受理し、保存は `advisor` に統一する。

route 名はユーザー定義の識別子であり、一括改称しない。
旧 `logical_model` / reasoning effort は先に既存 chain migration を通す。
移行結果は TOML 値と、スコープ（トップレベル / role profile）ごとの由来を持つ side-table を返す。
由来を示すキーは TOML 内へ挿入せず、ユーザー入力と生成物を構造的に分離する。
旧 `oracle` route + effort から生成した chain の由来は、同じ `models` 配列が上位層で置換された時点で捨てる
（配列内容が同一でも明示 chain として扱う）。部分的な binding の上書きでは由来を維持する。
新 `advisor` route の暗黙 chain は全層マージ後に確定し、別層・同一ファイルの明示 chain を上書きしない。
暗黙同士は層順に従い、下位 `oracle/low` → 上位 `advisor/high` は `advisor/high`、逆順は上位 `oracle/high` とする。
同じファイルに両 route があれば advisor を優先する。effort なしの上位 oracle route は旧仕様どおり chain を生成しない。
旧 oracle + effort が下位の明示 chain を上書きする既存一般挙動は維持する。
旧 oracle route の補完と新 advisor route の effort 解決は strict 検証より前に行い、単独保存では同じ最終化を一層に適用する。
プロジェクト層は `version` / `role_profile` のみを反映し、その移行由来は捨てる。
旧内部マーカー名と同名のユーザーフィールドも通常の未知キーとして警告 / strict 拒否する。
同名の合法な route / profile 識別子は保持する。内部由来は警告・エラー・ダンプ・GUI 設定 API・保存に含めない。
`role-oracle` preset の参照は受理し、同梱 fallback のみ新 Advisor 本文へ解決する。
明示的なユーザー `role-oracle.md` 上書きは保持するため、独自プロンプトの責務更新は利用者が必要に応じて行う。

delegate は #201 後もスキーマと parser の両方で `multimodallooker` alias を公開している。
これに合わせ `oracle` 入力を引き続き受理し、生成される run は Advisor に正規化する。
保存済み run/snapshot の `Oracle` / `oracle` と arena 評価帰属 `oracle` は読み込み alias として保持し、DB 全体の書換えは行わない。
Context inspector は旧名も Advisor として表示する。新規設定・GUI・標準プロンプト・評価帰属の保存は Advisor を使う。

## Validation and limits

権限、設定の strict load / save、移行順序・優先順位、delegate schema と実行、保存済み snapshot 復元、
GUI の表示、Advisor/Reviewer の用途案内を自動テストする。
この変更はプロンプト契約を明確にするもので、モデルが常に契約どおり振る舞うことや判断品質の向上を保証しない。
モデル挙動 A/B は別途評価する。
