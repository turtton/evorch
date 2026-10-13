# v14-f-allowed-directories-mounts Implementation Packet

## Goal

trust 済みの許可ディレクトリを runtime に渡し、shell とファイルツールの両方から、role の権限どおりのアクセス権で使えるようにする。

## Why

許可ディレクトリは GUI に登録できるのに、エージェントからは見えない。ADR 0033 でファイルツールに境界が入ると、許可ディレクトリを境界に加えない限り、昇格なしでは触れなくなる。

## Scope

- **宣言:** runtime に `set_project_allowed_directories(root, Vec<AllowedPath>)`（名前は実装で決める）を加える。`AllowedPath` はパスだけを持ち、アクセス権は runtime が run ごとに role から決める。GUI は `sync_project_trust` と同じ流れで、プロジェクトごとに `TrustState::Approved` の許可ディレクトリだけを渡し、宣言済みの内容と変わったときに宣言し直す。
- **保持:** runtime はプロジェクトのルートをキーに一覧を保持する。`set_project_trust` と違い、走行中の run にも変更を届ける必要があるので、run が開始時の値をコピーして持つのではなく、共有の状態を参照させる。
- **アクセス権:** run の `ExecutionPolicy` の許可ツールに `write` / `edit` / `shell` のいずれかが含まれれば読み書き可、そうでなければ読み取り専用とする。
- **境界一覧:** v14-e が作った共通の境界一覧に、許可ディレクトリを読み書き可 / 読み取り専用の領域として加える。bwrap のマウント（`bind` / `ro_bind`）とファイルツールの判定の両方がこの一覧を使う。
- **走行中の反映:** sandbox がマウント一覧を生成時の `BwrapConfig` に固定しないようにする。コマンドごとの `build_argv` で、その時点の許可ディレクトリを読む。ファイルツールの判定も呼び出しのたびに現在の一覧を読む。
- **Docs:** `docs/` に、許可ディレクトリの trust と role ごとのアクセス権を記載する。

### 採用・却下案と理由

- run の開始時に一覧をコピーする案は却下する。ADR 0032 は変更を走行中の run の次のコマンドから効かせることを求めており、削除や trust の取り消しがすぐ効かないと境界として信用できない。
- アクセス権を許可ディレクトリごとに GUI で設定する案は採らない（ADR 0032 は role に従うと決めた）。

## Out of scope

システムメッセージへの記載と途中変更の追記（v14-g）、ルールと skills の読み込み（v14-h）。

## Verification

### Unit

- 宣言から `Unapproved` が除かれること。
- role の許可ツールからアクセス権が決まること（Worker は読み書き可、Explorer / Reviewer などは読み取り専用）。
- 境界一覧への追加・削除と、ファイルツールの判定への反映。

### Integration → build

- 走行中の Worker の run で、許可ディレクトリを宣言した後の shell コマンドがそのディレクトリを読み書きでき、宣言から外した後は見えないこと。完了の検知はイベントやツール結果で行い、sleep に頼らない（test-policy）。
- 読み取り専用の role の run で、`git_diff` などからは読めるが書き込めないこと。
- `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、変更 crate の `cargo nextest run`、`scripts/check-cache-contracts.sh`、`git diff --check`。

### 未確認事項

- bwrap はマウント先が存在しないとき `--bind` で失敗する。存在しなくなった許可ディレクトリは argv から除く（`--ro-bind-try` / `--bind-try` を使うか事前に確認する）。
