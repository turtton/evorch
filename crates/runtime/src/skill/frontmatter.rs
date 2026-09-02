//! SKILL.md frontmatter の分割・解析・agentskills 仕様検証 (issue #53 / AC1)。
//!
//! 不変条件:
//! - エラー Display は識別子と理由のみを運び、skill 本文や frontmatter 値を
//!   漏らさない (`agent_loop` の system_prompt エラーと同一の規約)。
//! - フェンス行は LF 改行のみ受理し、CRLF は拒否する (決定事項と理由は
//!   [`split_frontmatter`] の doc を参照)。

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead};
use std::path::Path;

use serde::Deserialize;
use serde_yaml_ng::{Mapping, Value};

/// agentskills 仕様が許可する frontmatter フィールドの許可リスト。
const ALLOWED_FIELDS: [&str; 6] = [
    "name",
    "description",
    "allowed-tools",
    "license",
    "metadata",
    "compatibility",
];

/// name / description / license / allowed-tools / metadata / compatibility を
/// 保持する agentskills 仕様準拠の frontmatter。
///
/// `deny_unknown_fields` により、仕様の許可リスト外のフィールドは deserialize
/// 段階で拒否される。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillFrontmatter {
    /// スキル名。`^[a-z0-9]+(-[a-z0-9]+)*$` に一致し 64 文字以下。
    pub name: String,
    /// スキル説明。空でなく 1024 文字以下。
    pub description: String,
    /// ライセンス (任意)。
    #[serde(default)]
    pub license: Option<String>,
    /// 空白区切りの許可ツール一覧 (任意)。YAML 配列は不正。
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<String>,
    /// 任意の文字列 → 文字列マップ (任意)。非文字列値は検証エラー。
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
    /// 互換性表記 (任意)。500 文字以下。
    #[serde(default)]
    pub compatibility: Option<String>,
}

/// frontmatter 検証エラー。Display は識別子と理由のみを運ぶ。
#[derive(Debug, thiserror::Error)]
pub enum SkillValidationError {
    #[error("SKILL.md は先頭行が frontmatter フェンス `---` で始まる必要があります")]
    MissingLeadingFence,
    #[error("SKILL.md の frontmatter に閉じフェンス行 `---` がありません")]
    MissingClosingFence,
    #[error("SKILL.md の frontmatter が不正な YAML です: {0}")]
    InvalidYaml(String),
    #[error("SKILL.md の frontmatter に未知のフィールド '{0}' があります")]
    UnknownField(String),
    #[error("SKILL.md の frontmatter に必須フィールド 'name' がありません")]
    MissingName,
    #[error("SKILL.md の frontmatter フィールド 'name' は空でない文字列である必要があります")]
    EmptyName,
    #[error("SKILL.md の frontmatter フィールド 'name' が 64 文字を超えています")]
    NameTooLong,
    #[error(
        "SKILL.md の frontmatter フィールド 'name' は ^[a-z0-9]+(-[a-z0-9]+)*$ に一致する必要があります"
    )]
    InvalidNameFormat,
    #[error("SKILL.md の frontmatter フィールド 'name' がディレクトリ名 '{0}' と一致しません")]
    NameMismatch(String),
    #[error("SKILL.md の frontmatter に必須フィールド 'description' がありません")]
    MissingDescription,
    #[error(
        "SKILL.md の frontmatter フィールド 'description' は空でない文字列である必要があります"
    )]
    EmptyDescription,
    #[error("SKILL.md の frontmatter フィールド 'description' が 1024 文字を超えています")]
    DescriptionTooLong,
    #[error("SKILL.md の frontmatter フィールド 'compatibility' が 500 文字を超えています")]
    CompatibilityTooLong,
    #[error(
        "SKILL.md の frontmatter フィールド 'allowed-tools' は空白区切りの単一文字列である必要があります"
    )]
    InvalidAllowedTools,
    #[error(
        "SKILL.md の frontmatter フィールド 'metadata' は文字列キーと文字列値のマップである必要があります"
    )]
    InvalidMetadata,
    #[error("SKILL.md の frontmatter フィールド '{0}' は文字列である必要があります")]
    InvalidFieldType(&'static str),
}

/// SKILL.md 内容を frontmatter YAML 区間と本文に分割する。
///
/// フェンス規約: 内容はリテラル `---\n` で始まり、YAML 区間の後に閉じフェンス
/// 行 `---` (行全体) が続き、本文はその直後から末尾まで。CRLF は拒否する —
/// agentskills 仕様がフェンスを LF リテラルとして要求するため、Windows 由来の
/// `---\r\n` は暗黙に変換せず構造エラーとして扱う。
///
/// 戻り値は `(yaml 区間, 本文)`。YAML 区間には末尾改行を含まない。
pub fn split_frontmatter(content: &str) -> Result<(&str, &str), SkillValidationError> {
    let Some(after_opening) = content.strip_prefix("---\n") else {
        return Err(SkillValidationError::MissingLeadingFence);
    };
    if let Some(body) = after_opening.strip_prefix("---\n") {
        return Ok(("", body));
    }
    if after_opening == "---" {
        return Ok(("", ""));
    }
    if let Some((yaml, body)) = after_opening.split_once("\n---\n") {
        return Ok((yaml, body));
    }
    if let Some(yaml) = after_opening.strip_suffix("\n---") {
        return Ok((yaml, ""));
    }
    Err(SkillValidationError::MissingClosingFence)
}

/// discovery が SKILL.md から読み取る frontmatter 先頭部分の上限 (バイト数)。
/// 閉じフェンスが検出できないままこの上限を超えた SKILL.md は読み取り不可と
/// して扱い、異常に巨大な frontmatter による過剰なメモリ使用を防ぐ。
pub const FRONTMATTER_PREFIX_CAP: usize = 64 * 1024;

/// 閉じフェンスのバイト列。行頭の `---` とその直後の改行で構成される。
const CLOSING_FENCE: &[u8] = b"\n---\n";

/// SKILL.md から frontmatter 先頭部分のみを增量読み取りする (issue #53 / AC4)。
///
/// ファイルを先頭からチャンク単位で読み、閉じフェンス (`\n---\n`) を検出した
/// 時点で読み取りを打ち切り、ファイル先頭から閉じフェンスまで (含む) の部分の
/// みを返す。本文は String として実体化されない (progressive disclosure)。
/// 閉じフェンスが見つからず EOF に達した場合は読み取った全バイトを返す —
/// 閉じフェンスの欠落は下流の [`parse_and_validate`] が
/// `MissingClosingFence` として報告する。末尾が `\n---` で終わるファイルも
/// EOF 到達時に全バイトが返るため、閉じフェンス行として下流で受理される。
///
/// # Errors
/// - 閉じフェンスが検出できないまま [`FRONTMATTER_PREFIX_CAP`] バイトを超えた
///   場合: `ErrorKind::InvalidInput` ("frontmatter too large")。discovery 層で
///   読み取り不可として扱われる。
/// - 取得した先頭部分が正しい UTF-8 でない場合: `ErrorKind::InvalidData`。
///   frontmatter は UTF-8 を要求し、違反は観測可能にする (fail closed)。
pub fn read_frontmatter_prefix(path: &Path) -> std::io::Result<String> {
    let mut reader = io::BufReader::new(fs::File::open(path)?);
    let mut prefix: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        prefix.extend_from_slice(available);
        let chunk_len = available.len();
        reader.consume(chunk_len);
        if let Some(end) = find_closing_fence_end(&prefix) {
            prefix.truncate(end);
            break;
        }
        if prefix.len() > FRONTMATTER_PREFIX_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frontmatter too large",
            ));
        }
    }
    String::from_utf8(prefix).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "SKILL.md frontmatter is not valid UTF-8",
        )
    })
}

/// スライス中の最初の閉じフェンス (`\n---\n`) を探し、フェンス終端 (末尾
/// バイトの直後) のインデックスを返す。見つからなければ `None`。
fn find_closing_fence_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(CLOSING_FENCE.len())
        .position(|window| window == CLOSING_FENCE)
        .map(|position| position + CLOSING_FENCE.len())
}

/// SKILL.md 内容を解析し、agentskills 仕様に基づいて検証する。
///
/// `dir_name` は skill の親ディレクトリ名であり、frontmatter の `name` と
/// 一致しなければならない。検証順は構造 (フェンス/YAML) → 未知フィールド →
/// name 規約 → description 規約 → その他フィールド の固定順。
pub fn parse_and_validate(
    content: &str,
    dir_name: &str,
) -> Result<SkillFrontmatter, SkillValidationError> {
    let (yaml, _) = split_frontmatter(content)?;
    let value: Value = serde_yaml_ng::from_str(yaml)
        .map_err(|_| SkillValidationError::InvalidYaml("syntax error".to_owned()))?;
    let mapping = value.as_mapping().ok_or_else(|| {
        SkillValidationError::InvalidYaml("frontmatter root must be a mapping".to_owned())
    })?;

    for key in mapping.keys() {
        let Some(field) = key.as_str() else {
            return Err(SkillValidationError::InvalidYaml(
                "frontmatter field names must be strings".to_owned(),
            ));
        };
        if !ALLOWED_FIELDS.contains(&field) {
            return Err(SkillValidationError::UnknownField(field.to_owned()));
        }
    }

    require_string(mapping, "name", SkillValidationError::MissingName)?;
    require_string(
        mapping,
        "description",
        SkillValidationError::MissingDescription,
    )?;
    optional_string(mapping, "license")?;
    optional_string(mapping, "compatibility")?;
    validate_allowed_tools_type(mapping)?;
    validate_metadata_type(mapping)?;

    let frontmatter: SkillFrontmatter = serde_yaml_ng::from_value(value)
        .map_err(|_| SkillValidationError::InvalidYaml("invalid field value".to_owned()))?;
    validate_rules(&frontmatter, dir_name)?;
    Ok(frontmatter)
}

fn field<'a>(mapping: &'a Mapping, name: &str) -> Option<&'a Value> {
    mapping.get(Value::String(name.to_owned()))
}

fn require_string(
    mapping: &Mapping,
    name: &'static str,
    missing: SkillValidationError,
) -> Result<(), SkillValidationError> {
    let Some(value) = field(mapping, name) else {
        return Err(missing);
    };
    if value.is_string() {
        Ok(())
    } else {
        Err(SkillValidationError::InvalidFieldType(name))
    }
}

fn optional_string(mapping: &Mapping, name: &'static str) -> Result<(), SkillValidationError> {
    match field(mapping, name) {
        Some(value) if !value.is_string() => Err(SkillValidationError::InvalidFieldType(name)),
        Some(_) | None => Ok(()),
    }
}

fn validate_allowed_tools_type(mapping: &Mapping) -> Result<(), SkillValidationError> {
    match field(mapping, "allowed-tools") {
        Some(value) if !value.is_string() => Err(SkillValidationError::InvalidAllowedTools),
        Some(_) | None => Ok(()),
    }
}

fn validate_metadata_type(mapping: &Mapping) -> Result<(), SkillValidationError> {
    let Some(value) = field(mapping, "metadata") else {
        return Ok(());
    };
    let Some(metadata) = value.as_mapping() else {
        return Err(SkillValidationError::InvalidMetadata);
    };
    if metadata
        .iter()
        .all(|(key, value)| key.is_string() && value.is_string())
    {
        Ok(())
    } else {
        Err(SkillValidationError::InvalidMetadata)
    }
}

fn validate_rules(
    frontmatter: &SkillFrontmatter,
    dir_name: &str,
) -> Result<(), SkillValidationError> {
    if frontmatter.name.is_empty() {
        return Err(SkillValidationError::EmptyName);
    }
    if frontmatter.name.chars().count() > 64 {
        return Err(SkillValidationError::NameTooLong);
    }
    if !frontmatter.name.split('-').all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    }) {
        return Err(SkillValidationError::InvalidNameFormat);
    }
    if frontmatter.name != dir_name {
        return Err(SkillValidationError::NameMismatch(dir_name.to_owned()));
    }
    if frontmatter.description.is_empty() {
        return Err(SkillValidationError::EmptyDescription);
    }
    if frontmatter.description.chars().count() > 1024 {
        return Err(SkillValidationError::DescriptionTooLong);
    }
    if frontmatter
        .compatibility
        .as_ref()
        .is_some_and(|value| value.chars().count() > 500)
    {
        return Err(SkillValidationError::CompatibilityTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// `yaml` を LF フェンスで囲み、末尾に固定本文を付した SKILL.md 内容を作る。
    fn fenced(yaml: &str) -> String {
        format!("---\n{yaml}\n---\nBody.\n")
    }

    /// name/description に加えて追加 YAML 行を持つ frontmatter 本文を作る。
    fn skill_yaml(name: &str, description: &str, extra: &[&str]) -> String {
        let mut yaml = format!("name: {name}\ndescription: {description}\n");
        for line in extra {
            yaml.push_str(line);
            yaml.push('\n');
        }
        yaml
    }

    // -- split_frontmatter ---------------------------------------------------

    /// Given: 先頭フェンス + YAML + 閉じフェンス + 本文の SKILL.md
    /// When:  split_frontmatter を呼ぶ
    /// Then:  YAML 区間と本文が分離されて返る
    #[test]
    fn split_returns_yaml_and_body() {
        let (yaml, body) = split_frontmatter("---\nname: x\n---\nBody.\n").unwrap();
        assert_eq!(yaml, "name: x");
        assert_eq!(body, "Body.\n");
    }

    /// Given: 先頭フェンスで始まらない内容
    /// When:  split_frontmatter を呼ぶ
    /// Then:  MissingLeadingFence
    #[test]
    fn split_errors_when_leading_fence_missing() {
        let err = split_frontmatter("name: x\n---\n").unwrap_err();
        assert!(matches!(err, SkillValidationError::MissingLeadingFence));
    }

    /// Given: 閉じフェンス行を欠く内容
    /// When:  split_frontmatter を呼ぶ
    /// Then:  MissingClosingFence
    #[test]
    fn split_errors_when_closing_fence_missing() {
        let err = split_frontmatter("---\nname: x\n").unwrap_err();
        assert!(matches!(err, SkillValidationError::MissingClosingFence));
    }

    /// Given: CRLF 改行の SKILL.md (決定: 拒否 — 仕様のフェンスは LF リテラル)
    /// When:  split_frontmatter を呼ぶ
    /// Then:  MissingLeadingFence
    #[test]
    fn split_rejects_crlf_leading_fence() {
        let err = split_frontmatter("---\r\nname: x\r\n---\r\n").unwrap_err();
        assert!(matches!(err, SkillValidationError::MissingLeadingFence));
    }

    /// Given: 先頭フェンスは LF、閉じフェンスのみ CRLF
    /// When:  split_frontmatter を呼ぶ
    /// Then:  MissingClosingFence
    #[test]
    fn split_rejects_crlf_closing_fence() {
        let err = split_frontmatter("---\nname: x\n---\r\nBody.\n").unwrap_err();
        assert!(matches!(err, SkillValidationError::MissingClosingFence));
    }

    // -- read_frontmatter_prefix -----------------------------------------------

    /// Given: frontmatter 後の本文が不正な UTF-8 バイト列である SKILL.md
    /// When:  read_frontmatter_prefix を呼ぶ
    /// Then:  閉じフェンスまでの先頭部分のみが返り、本文バイトは読まれない
    #[test]
    fn prefix_stops_at_closing_fence_and_ignores_body() {
        let root = tempdir().unwrap();
        let path = root.path().join("SKILL.md");
        let mut content = b"---\nname: x\ndescription: y\n---\n".to_vec();
        content.extend([0xff_u8, 0xfe].repeat(64));
        fs::write(&path, content).unwrap();

        let prefix = read_frontmatter_prefix(&path).unwrap();

        assert_eq!(prefix, "---\nname: x\ndescription: y\n---\n");
    }

    /// Given: 閉じフェンスがファイル末尾にあり改行を伴わない SKILL.md
    /// When:  read_frontmatter_prefix を呼ぶ
    /// Then:  末尾の `---` を含むファイル全体が返り、検証も通る
    #[test]
    fn prefix_includes_closing_fence_without_trailing_newline() {
        let root = tempdir().unwrap();
        let path = root.path().join("SKILL.md");
        fs::write(&path, "---\nname: x\ndescription: y\n---").unwrap();

        let prefix = read_frontmatter_prefix(&path).unwrap();

        assert_eq!(prefix, "---\nname: x\ndescription: y\n---");
        assert!(parse_and_validate(&prefix, "x").is_ok());
    }

    /// Given: 行中に `---` を含む行と、行頭の閉じフェンスがある SKILL.md
    /// When:  read_frontmatter_prefix を呼ぶ
    /// Then:  行頭の閉じフェンスまで読み進める (行中の `---` はフェンスとみなさない)
    #[test]
    fn prefix_requires_closing_fence_at_start_of_line() {
        let root = tempdir().unwrap();
        let path = root.path().join("SKILL.md");
        fs::write(&path, "---\nname: x\ndescription: a --- b\n---\nBody.\n").unwrap();

        let prefix = read_frontmatter_prefix(&path).unwrap();

        assert_eq!(prefix, "---\nname: x\ndescription: a --- b\n---\n");
    }

    /// Given: 閉じフェンスを欠く SKILL.md
    /// When:  read_frontmatter_prefix を呼ぶ
    /// Then:  エラーにはならず読み取った全バイトが返る (欠落の報告は下流の検証)
    #[test]
    fn prefix_returns_all_bytes_when_closing_fence_missing() {
        let root = tempdir().unwrap();
        let path = root.path().join("SKILL.md");
        fs::write(&path, "---\nname: x\ndescription: y\n").unwrap();

        let prefix = read_frontmatter_prefix(&path).unwrap();

        assert_eq!(prefix, "---\nname: x\ndescription: y\n");
    }

    /// Given: 閉じフェンスを欠くまま FRONTMATTER_PREFIX_CAP を超える SKILL.md
    /// When:  read_frontmatter_prefix を呼ぶ
    /// Then:  InvalidInput ("frontmatter too large") で異常終了する
    #[test]
    fn prefix_errors_when_cap_exceeded_without_closing_fence() {
        let root = tempdir().unwrap();
        let path = root.path().join("SKILL.md");
        let content = format!("---\nname: x\n{}", "a".repeat(FRONTMATTER_PREFIX_CAP + 1));
        fs::write(&path, content).unwrap();

        let err = read_frontmatter_prefix(&path).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("frontmatter too large"));
    }

    /// Given: 内容全体が不正な UTF-8 バイト列である SKILL.md
    /// When:  read_frontmatter_prefix を呼ぶ
    /// Then:  InvalidData で異常終了する (fail closed)
    #[test]
    fn prefix_errors_when_prefix_is_not_utf8() {
        let root = tempdir().unwrap();
        let path = root.path().join("SKILL.md");
        fs::write(&path, [0xff_u8, 0xfe, 0x00]).unwrap();

        let err = read_frontmatter_prefix(&path).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// Given: frontmatter 区間の途中に不正な UTF-8 バイトを含む SKILL.md
    /// When:  read_frontmatter_prefix を呼ぶ
    /// Then:  閉じフェンスの有無によらず InvalidData で異常終了する
    #[test]
    fn prefix_errors_when_frontmatter_region_is_not_utf8() {
        let root = tempdir().unwrap();
        let path = root.path().join("SKILL.md");
        let mut content = b"---\nname: ".to_vec();
        content.extend_from_slice(&[0xff, 0xfe]);
        content.extend_from_slice(b"\n---\nBody.\n");
        fs::write(&path, content).unwrap();

        let err = read_frontmatter_prefix(&path).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // -- parse_and_validate: 正常系 --------------------------------------------

    /// Given: 6 フィールドすべてを備え、name がディレクトリ名と一致する SKILL.md
    /// When:  parse_and_validate を呼ぶ
    /// Then:  全フィールドが保持された SkillFrontmatter が返る
    #[test]
    fn parses_full_featured_skill() {
        let yaml = "\
name: skill-loader
description: Loads and validates SKILL.md files
compatibility: \">=1.2.0\"
allowed-tools: Bash Read Grep
license: MIT
metadata:
  version: \"1.0\"
  author: evorch
";
        let fm = parse_and_validate(&fenced(yaml), "skill-loader").unwrap();
        assert_eq!(fm.name, "skill-loader");
        assert_eq!(fm.description, "Loads and validates SKILL.md files");
        assert_eq!(fm.compatibility.as_deref(), Some(">=1.2.0"));
        assert_eq!(fm.allowed_tools.as_deref(), Some("Bash Read Grep"));
        assert_eq!(fm.license.as_deref(), Some("MIT"));
        let metadata = fm.metadata.unwrap();
        assert_eq!(metadata.get("version").map(String::as_str), Some("1.0"));
        assert_eq!(metadata.get("author").map(String::as_str), Some("evorch"));
    }

    /// Given: 必須 2 フィールドのみの SKILL.md
    /// When:  parse_and_validate を呼ぶ
    /// Then:  任意フィールドはすべて None
    #[test]
    fn parses_minimal_skill() {
        let content = fenced(&skill_yaml("minimal-skill", "Minimal skill", &[]));
        let fm = parse_and_validate(&content, "minimal-skill").unwrap();
        assert_eq!(fm.name, "minimal-skill");
        assert_eq!(fm.description, "Minimal skill");
        assert!(fm.license.is_none());
        assert!(fm.allowed_tools.is_none());
        assert!(fm.metadata.is_none());
        assert!(fm.compatibility.is_none());
    }

    /// Given: name 64 文字 / description 1024 文字 (マルチバイト) / compatibility 500 文字
    /// When:  parse_and_validate を呼ぶ
    /// Then:  上限は含むため受理される (文字数は chars().count() で数える)
    #[test]
    fn accepts_boundary_lengths() {
        let name = "a".repeat(64);
        let description = "あ".repeat(1024);
        let compatibility = "c".repeat(500);
        let yaml =
            format!("name: {name}\ndescription: {description}\ncompatibility: {compatibility}\n");
        let fm = parse_and_validate(&fenced(&yaml), &name).unwrap();
        assert_eq!(fm.name, name);
        assert_eq!(fm.description, description);
        assert_eq!(fm.compatibility.as_deref(), Some(compatibility.as_str()));
    }

    // -- parse_and_validate: 構造エラー -----------------------------------------

    /// Given: YAML として構文不正な frontmatter
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidYaml であり、エラー文言に frontmatter 値が漏れない
    #[test]
    fn errors_on_invalid_yaml_syntax() {
        let content = "---\ndescription: LEAKMARKER oops: colon\nname: skill-loader\n---\nBody.\n";
        let err = parse_and_validate(content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidYaml(_)));
        assert!(!err.to_string().contains("LEAKMARKER"));
    }

    /// Given: 許可リスト外のフィールドを含む frontmatter
    /// When:  parse_and_validate を呼ぶ
    /// Then:  UnknownField(そのフィールド名)
    #[test]
    fn errors_on_unknown_field() {
        let content = fenced(&skill_yaml("skill-loader", "desc", &["bogus: 1"]));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::UnknownField(field) if field == "bogus"));
    }

    /// Given: 未知フィールドを含む YAML を serde 経由で構造体へ直接渡す
    /// When:  from_value する
    /// Then:  deny_unknown_fields 属性によりエラーになる (構造体側の契約)
    #[test]
    fn skill_frontmatter_serde_rejects_unknown_fields() {
        let value: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("name: x\ndescription: y\nbogus: 1").unwrap();
        assert!(serde_yaml_ng::from_value::<SkillFrontmatter>(value).is_err());
    }

    // -- name 検証 -------------------------------------------------------------

    /// Given: name を欠く frontmatter
    /// When:  parse_and_validate を呼ぶ
    /// Then:  MissingName
    #[test]
    fn errors_when_name_missing() {
        let content = fenced("description: desc\n");
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::MissingName));
    }

    /// Given: name が空文字列
    /// When:  parse_and_validate を呼ぶ
    /// Then:  EmptyName
    #[test]
    fn errors_when_name_empty() {
        let content = fenced("name: \"\"\ndescription: desc\n");
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::EmptyName));
    }

    /// Given: name が 65 文字
    /// When:  parse_and_validate を呼ぶ
    /// Then:  NameTooLong
    #[test]
    fn errors_when_name_exceeds_64_chars() {
        let name = "a".repeat(65);
        let content = fenced(&skill_yaml(&name, "desc", &[]));
        let err = parse_and_validate(&content, &name).unwrap_err();
        assert!(matches!(err, SkillValidationError::NameTooLong));
    }

    /// Given: name に大文字を含む
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidNameFormat
    #[test]
    fn errors_when_name_has_uppercase() {
        let content = fenced(&skill_yaml("Skill-Loader", "desc", &[]));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidNameFormat));
    }

    /// Given: name に非 ASCII 文字を含む
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidNameFormat
    #[test]
    fn errors_when_name_has_unicode() {
        let content = fenced(&skill_yaml("skïll", "desc", &[]));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidNameFormat));
    }

    /// Given: name がハイフンで始まる
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidNameFormat
    #[test]
    fn errors_when_name_starts_with_hyphen() {
        let content = fenced(&skill_yaml("-skill", "desc", &[]));
        let err = parse_and_validate(&content, "-skill").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidNameFormat));
    }

    /// Given: name がハイフンで終わる
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidNameFormat
    #[test]
    fn errors_when_name_ends_with_hyphen() {
        let content = fenced(&skill_yaml("skill-", "desc", &[]));
        let err = parse_and_validate(&content, "skill-").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidNameFormat));
    }

    /// Given: name が連続ハイフンを含む
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidNameFormat
    #[test]
    fn errors_when_name_has_consecutive_hyphens() {
        let content = fenced(&skill_yaml("a--b", "desc", &[]));
        let err = parse_and_validate(&content, "a--b").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidNameFormat));
    }

    /// Given: name は妥当だがディレクトリ名と不一致
    /// When:  parse_and_validate を呼ぶ
    /// Then:  NameMismatch(ディレクトリ名)
    #[test]
    fn errors_when_name_differs_from_directory() {
        let content = fenced(&skill_yaml("skill-loader", "desc", &[]));
        let err = parse_and_validate(&content, "other-skill").unwrap_err();
        assert!(matches!(err, SkillValidationError::NameMismatch(dir) if dir == "other-skill"));
    }

    /// Given: name が文字列ではない (数値)
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidFieldType("name")
    #[test]
    fn errors_when_name_is_not_a_string() {
        let content = fenced("name: 5\ndescription: desc\n");
        let err = parse_and_validate(&content, "5").unwrap_err();
        assert!(matches!(
            err,
            SkillValidationError::InvalidFieldType("name")
        ));
    }

    // -- description 検証 --------------------------------------------------------

    /// Given: description を欠く frontmatter
    /// When:  parse_and_validate を呼ぶ
    /// Then:  MissingDescription
    #[test]
    fn errors_when_description_missing() {
        let content = fenced("name: skill-loader\n");
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::MissingDescription));
    }

    /// Given: description が空文字列
    /// When:  parse_and_validate を呼ぶ
    /// Then:  EmptyDescription
    #[test]
    fn errors_when_description_empty() {
        let content = fenced("name: skill-loader\ndescription: \"\"\n");
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::EmptyDescription));
    }

    /// Given: description が 1025 文字
    /// When:  parse_and_validate を呼ぶ
    /// Then:  DescriptionTooLong
    #[test]
    fn errors_when_description_exceeds_1024_chars() {
        let content = fenced(&skill_yaml("skill-loader", &"d".repeat(1025), &[]));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::DescriptionTooLong));
    }

    // -- その他のフィールド --------------------------------------------------------

    /// Given: compatibility が 501 文字
    /// When:  parse_and_validate を呼ぶ
    /// Then:  CompatibilityTooLong
    #[test]
    fn errors_when_compatibility_exceeds_500_chars() {
        let line = format!("compatibility: {}", "c".repeat(501));
        let content = fenced(&skill_yaml("skill-loader", "desc", &[&line]));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::CompatibilityTooLong));
    }

    /// Given: allowed-tools が YAML 配列
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidAllowedTools
    #[test]
    fn errors_when_allowed_tools_is_array() {
        let content = fenced(&skill_yaml(
            "skill-loader",
            "desc",
            &["allowed-tools: [Bash, Read]"],
        ));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidAllowedTools));
    }

    /// Given: metadata の値が文字列以外 (数値)
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidMetadata
    #[test]
    fn errors_when_metadata_value_is_not_string() {
        let content = fenced(&skill_yaml(
            "skill-loader",
            "desc",
            &["metadata:", "  count: 5"],
        ));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidMetadata));
    }

    /// Given: metadata のキーが文字列以外 (数値)
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidMetadata
    #[test]
    fn errors_when_metadata_key_is_not_string() {
        let content = fenced(&skill_yaml(
            "skill-loader",
            "desc",
            &["metadata:", "  1: x"],
        ));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(err, SkillValidationError::InvalidMetadata));
    }

    /// Given: license が文字列以外 (配列)
    /// When:  parse_and_validate を呼ぶ
    /// Then:  InvalidFieldType("license")
    #[test]
    fn errors_when_license_is_not_string() {
        let content = fenced(&skill_yaml("skill-loader", "desc", &["license: [MIT]"]));
        let err = parse_and_validate(&content, "skill-loader").unwrap_err();
        assert!(matches!(
            err,
            SkillValidationError::InvalidFieldType("license")
        ));
    }
}
