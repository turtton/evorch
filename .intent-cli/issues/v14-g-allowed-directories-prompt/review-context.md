# v14-g-allowed-directories-prompt Review Context

## Position and scope

ADR 0032 の 3 段のうち ②。許可ディレクトリをプロンプトで伝える部分だけを扱う。受け渡しとマウントは v14-f（前提）、ルールと skills の読み込みは v14-h であり、この PR に含まれないことを不足として扱わない。

## Intent references

- `intents/evorch/decisions/0032-project-allowed-directories.md`
- `intents/evorch/decisions/0003-cache-first-context-engine.md`
- `intents/evorch/decisions/0027-restore-contract.md`
- `intents/evorch/features/context-engine/overview.md`

## Review focus

### cache の契約

- 開始後にシステムメッセージを書き換える経路がないか。復元時の `update_workspace_system_message` に許可ディレクトリの節が巻き込まれていないか。
- 追記が、送信済みのメッセージの編集ではなく末尾への追加になっているか。未完了の ToolUse の間に割り込んでいないか。
- contract テストが、固定の cached-token ではなく wire prefix の照合で保存を確かめているか。AGENTS.md の Cache correctness gate を弱めていないか。

### 差分と状態

- 変化がないときに追記しないか。同じ差分を二重に追記しないか。
- 最後に伝えた一覧が、追記・復元・compaction の後で正しく更新されるか。
- 子 run が、親が伝えた一覧ではなく開始時点の現在の一覧を持つか。

### inspector

- base context の preview と実際のシステムメッセージで、許可ディレクトリの節が一致するか。

## Acceptance-to-evidence map

packet.yaml の acceptance_criteria の各項目に対応するテスト名を PR 本文に列挙する。
