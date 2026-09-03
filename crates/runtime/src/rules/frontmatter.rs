//! スコープ付きルールの YAML frontmatter 解析。

use std::path::Path;

use serde::Deserialize;

use super::types::{RuleMeta, RulesError};

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Frontmatter {
    #[serde(rename = "alwaysApply", alias = "always_apply")]
    always_apply: bool,
    #[serde(default)]
    globs: GlobValues,
    #[serde(default)]
    glob: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
enum GlobValues {
    One(String),
    Many(Vec<String>),
    #[default]
    Empty,
}

impl GlobValues {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(glob) => vec![glob],
            Self::Many(globs) => globs,
            Self::Empty => Vec::new(),
        }
    }
}

pub(crate) fn parse_frontmatter(path: &Path, raw: &str) -> Result<RuleMeta, RulesError> {
    if !raw.starts_with("---\n") {
        return Ok(RuleMeta::default());
    }
    let (yaml, _) = split_frontmatter(raw).ok_or_else(|| RulesError::InvalidFrontmatter {
        path: path.to_path_buf(),
    })?;
    let parsed: Frontmatter =
        serde_yaml_ng::from_str(yaml).map_err(|_| RulesError::InvalidFrontmatter {
            path: path.to_path_buf(),
        })?;
    let mut globs = parsed.globs.into_vec();
    if let Some(glob) = parsed.glob {
        globs.push(glob);
    }
    Ok(RuleMeta {
        always_apply: parsed.always_apply,
        globs,
    })
}

pub(crate) fn body_without_frontmatter(raw: &str) -> &str {
    split_frontmatter(raw).map_or(raw, |(_, body)| body)
}

fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("---\n")?;
    if let Some(boundary) = rest.find("\n---\n") {
        return Some((&rest[..boundary], &rest[boundary + 5..]));
    }
    rest.strip_suffix("\n---").map(|yaml| (yaml, ""))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::parse_frontmatter;

    // Given: alwaysApply が true・false・省略の frontmatter / When: 解析 / Then: 対応する真偽値になる
    #[test]
    fn parses_always_apply_variants() {
        assert!(
            parse_frontmatter(Path::new("a.md"), "---\nalwaysApply: true\n---\nbody")
                .expect("解析できる")
                .always_apply
        );
        assert!(
            !parse_frontmatter(Path::new("b.md"), "---\nalwaysApply: false\n---\nbody")
                .expect("解析できる")
                .always_apply
        );
        assert!(
            !parse_frontmatter(Path::new("c.md"), "---\nglobs: '*.rs'\n---\nbody")
                .expect("解析できる")
                .always_apply
        );
    }

    // Given: 文字列・配列・glob 別名の指定 / When: 解析 / Then: globs 配列へ正規化される
    #[test]
    fn normalizes_glob_forms() {
        assert_eq!(
            parse_frontmatter(Path::new("a.md"), "---\nglobs: '*.rs'\n---\n")
                .expect("解析できる")
                .globs,
            ["*.rs"]
        );
        assert_eq!(
            parse_frontmatter(Path::new("b.md"), "---\nglobs: ['*.rs', 'src/**']\n---\n")
                .expect("解析できる")
                .globs,
            ["*.rs", "src/**"]
        );
        assert_eq!(
            parse_frontmatter(Path::new("c.md"), "---\nglob: '*.md'\n---\n")
                .expect("解析できる")
                .globs,
            ["*.md"]
        );
    }

    // Given: 不正 YAML または未知キー / When: 解析 / Then: fail-closed でエラーになる
    #[test]
    fn rejects_invalid_or_unknown_frontmatter() {
        assert!(parse_frontmatter(Path::new("a.md"), "---\nglobs: [\n---\n").is_err());
        assert!(parse_frontmatter(Path::new("b.md"), "---\nunknown: true\n---\n").is_err());
        assert!(parse_frontmatter(Path::new("c.md"), "---\nalwaysApply: true\nbody").is_err());
    }

    // Given: 先頭 fence がない本文 / When: 解析 / Then: 既定メタデータになる
    #[test]
    fn no_fence_returns_default_meta() {
        assert_eq!(
            parse_frontmatter(Path::new("a.md"), "body").expect("既定値になる"),
            Default::default()
        );
    }

    // Given: frontmatter と本文 / When: 本文を分離 / Then: fence より後だけが返る
    #[test]
    fn body_excludes_frontmatter() {
        assert_eq!(
            super::body_without_frontmatter("---\nalwaysApply: true\n---\nbody"),
            "body"
        );
    }

    // Given: fence に余分な文字が続く本文 / When: frontmatter を解析 / Then: 不正 frontmatter として拒否される
    #[test]
    fn closing_fence_must_be_exact_line() {
        assert!(
            parse_frontmatter(Path::new("a.md"), "---\nalwaysApply: true\n----\nbody").is_err()
        );
    }
}
