# v01-config-secret-field-rejection Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- `deny_unknown_fields` 相当が Config root だけでなく provider/routing/panel/diagnostics/permissions/metrics の nested typed structs 全体に届き、nested typo が silent ignore されないか
- plaintext credential-like field が generic unknown-field error だけで終わらず、config path と Keyring/Env の正しい remediation を返すか
- `api_key` だけの場当たり的対応ではなく、packet で定めた hyphen/alias/token/secret/password/credential_value 等の正規集合が test で固定されているか。過度に一般的な field（例: provider type の正規 field）を誤拒否していないか
- `CredentialRefConfig::Keyring` と `Env` の variant 内でも余分な secret field を拒否し、Env の正規 field が `var` であるか
- user/project/drop-in/CLI の deep merge 後に strict typed parse が走り、source によって rejection を迂回できないか
- valid Keyring/Env、version migration、deep merge、既存 config sections の regression tests が通るか
- runtime keyring/env resolution や provider auth injection を変更していないか。scope widening の目印は resolver/provider crate の behavioral change
- JSON Schema publication、secret scanner、自動 rewrite/migration を追加していないか。これらは別 scope

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が config security boundary と actionable error の意味検証を担う。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/provider-routing/overview.md`: strict rejection を適用した type tree と plaintext credential non-storage invariant
- `operations/config-reference.md`: unknown key は無視されず error、credential は Keyring/Env reference のみ、Env form は `var` field
- credential-like field の正規 denylist/aliases と actionable error wording（path + remediation）

記録が未実施の場合は、canonical docs が再び insecure な silent-ignore behavior を案内するため、知識 writeback 不足として review 所見に残す。
