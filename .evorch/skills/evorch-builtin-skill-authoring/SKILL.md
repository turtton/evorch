---
name: evorch-builtin-skill-authoring
description: evorchの組み込みskillを追加・更新するときに参照する。SKILL.mdの作成、静的テーブルへの登録、関連リソースの同梱、探索・読み込み・上書き・プロンプトキャッシュのテスト、再ビルドまでの実装手順を確認する。evorch開発専用であり、汎用builtinとして配布しない。
---
# evorch: 組み込みskillの作成

このskillはevorch開発専用の技術ガイド。配置先は `.evorch/skills/evorch-builtin-skill-authoring/SKILL.md` とし、組み込みテーブルには登録しない。汎用builtinの内容へevorch固有の作業規則・APIを混ぜない。Git操作や公開は、別途適用される指示・規則・許可に従う。

## 1. 現在の実装を確認する

変更前に、以下の該当部分を確認する。このガイドと実装が異なる場合は、実装を確認してガイドも更新する。

| 対象 | 正本となる場所 |
|---|---|
| builtinの静的定義とSKILL.mdからの変換 | `crates/runtime/src/skill/builtin.rs` |
| frontmatter検証・本文分割 | `crates/runtime/src/skill/frontmatter.rs` |
| 探索順・builtinのマージ | `crates/runtime/src/skill/discovery.rs` |
| 読み込み元と本文・リソースのAPI | `crates/runtime/src/skill/registry.rs`、`resource.rs` |
| `skill_load` のハンドラ | `crates/runtime/src/meta/skills.rs` |
| run開始時のレジストリ・カタログ更新 | `crates/runtime/src/skill_source.rs` |
| 起動時の接続 | `crates/runtime/src/compose.rs` |
| `load_skills` による子runへの注入 | `crates/runtime/src/meta/delegation.rs`、`agent_loop.rs` |

既存の `crates/runtime/skills/builtin/git-best-practices/SKILL.md` と `crates/runtime/tests/builtin_git_skill.rs` は、コンテンツと利用経路の実例。

## 2. 配布範囲を決める

- **汎用builtin**: evorchを使う他のプロジェクトでも役立つ内容。`crates/runtime/skills/builtin/<name>/` に配置し、バイナリに同梱する。
- **evorch開発専用skill**: このガイドのような実装手順・repo固有情報。`.evorch/skills/<name>/` に配置する。全ユーザー向けbuiltinには登録しない。
- builtin本文にevorch固有のロール名、ツール呼び出し構文、リポジトリ方針を入れない。必要な利用方法は `crates/runtime/skills/builtin/README.md` 等の開発文書へ分離する。
- skillは指針であり、ツール権限の拡張、実行許可、強制チェックの代わりにはならない。

## 3. SKILL.mdを作る

配置例:

```text
crates/runtime/skills/builtin/example-skill/
├── SKILL.md
└── references/
    └── guide.md
```

SKILL.mdはUTF-8・LF改行で作成する。先頭に空行やBOMを置かず、YAML frontmatterの `---` フェンスから始める。

```markdown
---
name: example-skill
description: このskillを参照すべき状況と、得られる指針を具体的に説明する。
---
# Example skill

必要な手順・判断基準・制限をここに記載する。
```

現在の検証規則:

- `name` と `description` は必須の空でない文字列。
- `name` は `^[a-z0-9]+(-[a-z0-9]+)*$`、64文字以下、ディレクトリ名と一致。
- `description` は1024文字以下。「いつ使うか」を明示する。発見時のモデル向け情報は名前と説明だけなので、本文を読まないと用途が分からない説明を避ける。
- 任意フィールドは `license`、`compatibility`、`allowed-tools`、`metadata`。未知フィールドは拒否される。
- `compatibility` は500文字以下。`allowed-tools` は配列でなく単一文字列。`metadata` は文字列キー・文字列値のマップ。
- `allowed-tools` は現状、権限へ適用されない。実際の権限はロールのcapability等で決まる。

本文は必要十分に短く保つ。長い補足は関連リソースへ分け、本文に相対参照と参照すべき条件を書く。秘密情報やローカル絶対パスを同梱しない。

## 4. バイナリへ登録する

`crates/runtime/src/skill/builtin.rs` の `BUILTIN_SKILLS` に `EmbeddedSkillDef` を追加する。

```rust
EmbeddedSkillDef {
    name: "example-skill",
    skill_md: include_str!("../../skills/builtin/example-skill/SKILL.md"),
    resources: &[
        (
            "references/guide.md",
            include_str!("../../skills/builtin/example-skill/references/guide.md"),
        ),
    ],
},
```

- `name` はSKILL.mdとディレクトリ名に一致させる。既存builtin名と重複させない。
- **SKILL.mdを正本にする。** `description` や本文をRust側へ複製しない。`to_entry()` がfrontmatterを検証し、本文を分割して `SkillSource::Embedded` を作る。
- 現在の `to_entry()` は不正な同梱frontmatterでpanicするため、登録内容の検証テストを必ず実行する。ローカルファイルへの読み込みフォールバックを追加しない。
- リソースなしなら `resources: &[]`。関連ファイルを置くだけでは同梱されず、上記テーブルへの登録が必要。
- リソースキーはskillルート相対の `file` または `dir/file` まで。空文字、絶対パス、`.`、`..`、バックスラッシュ、2階層以上の参照は拒否される。
- 同梱内容はUTF-8テキスト。スクリプトも読み込みで返すだけで自動実行しない。
- `crates/runtime/skills/builtin/README.md` の利用可能skill一覧も更新する。

## 5. 実行時の契約を維持する

- 標準の優先順位は **repo `.evorch/skills` → repo `.agents/skills` → user config `evorch/skills` → `$HOME/.agents/skills` → builtin**。同名の有効なfilesystem skillがbuiltinを上書きし、Shadowed診断が出る。user configは `$XDG_CONFIG_HOME`（未設定・空なら `$HOME/.config`）を使う。user-agentsは空でない `$HOME` がある場合だけ探索し、XDGからHOMEを推測しない。存在しない探索先は許容する。
- 通常のGUI等が使う `compose_runtime()` はスキル供給元を接続する。repoスコープの探索にはrepo rootが必要。
- 発見・カタログ構築では名前と説明を公開する。builtin本文を全runのSystemへ無条件に追加しない。
- 本文は `skill_load({"name":"example-skill"})`、リソースは `skill_load({"name":"example-skill","resource":"references/guide.md"})` で取得できる。
- `skill_load` を利用できるロールは現在Orchestrator、Worker、Planner。Git作業等の委譲で事前読み込みが必要なら `delegate` の `load_skills` に登録名を指定する。
- `load_skills` は初期Systemの `## Skills` へ本文を注入する。frontmatterは含めない。
- filesystemの一覧・説明はrun開始境界で再探索し、本文・リソースは読み込み時に取得する。builtinの変更は**再ビルド・新しいバイナリの起動が必要**。
- 実行中・継続中の会話の送信済みSystem、ツール定義、過去のツール結果をskill更新で書き換えない。新規runへの反映と、既存会話のプロンプトキャッシュ保持を分ける。

## 6. 検証を追加して実行する

新しいskillについて、少なくとも次を確認する。

1. 正本のfrontmatterが有効で、テーブルの名前・説明・本文と一致する。本文からfrontmatterが除かれる。
2. builtinが外部のskillファイルなしで発見・読み込みでき、必要なリソースが正しいキーで取得できる。
3. カタログには名前・説明だけが現れ、必要時の `skill_load` と `load_skills` が実ループで本文を取得できる。
4. repo、repo-agents、user、user-agentsの同名skillがbuiltinより優先される。
5. 不正なリソース参照・未知名が拒否される。汎用skillなら、プロジェクト固有文言が紛れ込んでいない。
6. キャッシュへ影響する変更では、入力由来のmock cacheと独立したwire-prefix検証を使う。固定のcached-token値や「本文がある」という確認だけで代替しない。

現在の基準となる検証コマンド（repo rootで実行）:

```sh
cargo fmt --all --check
cargo test -p runtime --lib skill::
cargo test -p runtime --test builtin_git_skill --test skill_hot_reload --test skill_hot_reload_cache --test skill_surface --test skill_delegation --test repo_skill_authoring
cargo clippy --workspace --all-targets -- -D warnings
scripts/check-cache-contracts.sh
```

新規の結合テストを作った場合は、そのtargetも実行する。キャッシュに影響するシナリオは `scripts/check-cache-contracts.sh` の対象へ追加し、既存契約を弱めない。

この開発専用skill自体も、frontmatter、repoスコープでの発見・本文読み込み、builtinに混入しないことを `crates/runtime/tests/repo_skill_authoring.rs` で検証する。

## 完了条件

- 配布範囲に合った配置で、正本・登録・文書・テストが揃っている。
- 検証結果と、再ビルドが必要なことを報告する。
- merge・push・CI確認が依頼されている場合は、適用規則と許可に従って行い、該当commit SHAのCI結果まで確認する。
