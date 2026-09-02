//! SKILL.md メタデータレジストリ (issue #53 / AC3, AC7)。
//!
//! レジストリは frontmatter 由来のメタデータのみを保持し、本文は
//! [`SkillRegistry::load_body`] で都度ディスクから読み直す (progressive
//! disclosure)。エラー Display は識別子のみを運び、本文や frontmatter 値を
//! 漏らさない (ADR 0010 / frontmatter モジュールと同一規約)。

use std::collections::BTreeMap;
use std::path::PathBuf;

use event_bus::SkillDiagnosticKind;

use super::frontmatter::split_frontmatter;
use crate::prompt::AvailableSkill;

/// skill の由来スコープ。優先順位は repo > user (discovery 側で処理順に反映)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// リポジトリスコープ (`<repo>/.evorch/skills`)。
    Repo,
    /// ユーザスコープ (`<user config>/evorch/skills`)。
    User,
}

impl SkillScope {
    /// 診断 detail やイベントペイロード用の固定識別子。
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillScope::Repo => "repo",
            SkillScope::User => "user",
        }
    }
}

/// 発見済み skill のメタデータ 1 件分。本文 (SKILL.md の内容) は保持しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    /// frontmatter の `name`。ディレクトリ名との一致は発見時に検証済み。
    pub name: String,
    /// frontmatter の `description`。
    pub description: String,
    /// SKILL.md を含む skill ディレクトリ。
    pub dir: PathBuf,
    /// この entry の由来スコープ。
    pub scope: SkillScope,
}

/// skill 発見時の診断 1 件分。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDiagnostic {
    /// 診断の種別。
    pub kind: SkillDiagnosticKind,
    /// 対象 skill 名。frontmatter の name が復元できなければディレクトリ名。
    pub skill: String,
    /// 診断対象のスコープ。
    pub scope: SkillScope,
    /// 識別子と理由のみを運び、skill 本文は含まない。
    pub detail: String,
}

/// 発見済み skill のメタデータレジストリ。name で一意に持つ (BTreeMap により
/// name 昇順の走査順が保証される)。
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: BTreeMap<String, SkillEntry>,
    /// 発見時に生成された診断 (処理順。重複排除はしない)。
    pub diagnostics: Vec<SkillDiagnostic>,
}

impl SkillRegistry {
    /// [`discover_skills`](super::discovery::discover_skills) が使う内部
    /// コンストラクタ。skills のキーは entry.name と一致していなければならない。
    pub(crate) fn new(
        skills: BTreeMap<String, SkillEntry>,
        diagnostics: Vec<SkillDiagnostic>,
    ) -> Self {
        Self {
            skills,
            diagnostics,
        }
    }

    /// 指定名の entry を返す。
    pub fn get(&self, name: &str) -> Option<&SkillEntry> {
        self.skills.get(name)
    }

    /// 利用可能 skill を name 昇順で返す。
    pub fn available_skills(&self) -> Vec<AvailableSkill> {
        self.skills
            .values()
            .map(|entry| AvailableSkill {
                name: entry.name.clone(),
                description: entry.description.clone(),
            })
            .collect()
    }

    /// 登録済み skill が 1 つもないか。
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// 登録済み skill 数。
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// skill 本文 (frontmatter 直後から末尾まで) をディスクから読み直す。
    ///
    /// progressive disclosure のため本文は発見時に保持せず、この場で
    /// `<dir>/SKILL.md` を読む。発見時の検証は繰り返さない (frontmatter の
    /// name とディレクトリ名の一致は再確認しない)。
    ///
    /// # Errors
    /// 未登録名なら [`SkillLoadError::UnknownSkill`]、SKILL.md が読めなければ
    /// [`SkillLoadError::Unreadable`]、frontmatter 構造の分割に失敗すれば
    /// [`SkillLoadError::Corrupt`]。
    pub fn load_body(&self, name: &str) -> Result<String, SkillLoadError> {
        let Some(entry) = self.skills.get(name) else {
            return Err(SkillLoadError::UnknownSkill(name.to_owned()));
        };
        let content = std::fs::read_to_string(entry.dir.join("SKILL.md")).map_err(|_| {
            SkillLoadError::Unreadable {
                name: entry.name.clone(),
            }
        })?;
        let (_, body) = split_frontmatter(&content).map_err(|_| SkillLoadError::Corrupt {
            name: entry.name.clone(),
        })?;
        Ok(body.to_owned())
    }
}

/// load 済み skill 本文リストを `## Skills` セクション文字列へレンダリングする
/// (issue #53 / AC6)。
///
/// 各 skill はマーカー fence で括られ、要求順を保持する (name ソートは
/// 行わない)。マーカー形式は [`crate::prompt`] の keyTriggers 埋め込みと
/// 同規約 (`<!-- ... BEGIN/END -->`) に揃える。決定的: 同一入力からは常に
/// バイト同一の出力を返す。
pub fn render_skills_section(loaded: &[(String, String)]) -> String {
    let mut section = String::from("## Skills");
    for (name, body) in loaded {
        section.push_str(&format!(
            "\n\n<!-- skill:{name} BEGIN -->\n{body}\n<!-- skill:{name} END -->"
        ));
    }
    section
}

/// [`SkillRegistry::load_body`] の型付きエラー。Display は識別子のみを運ぶ。
#[derive(Debug, thiserror::Error)]
pub enum SkillLoadError {
    /// 指定名の skill はレジストリに存在しない。
    #[error("skill '{0}' は登録されていません")]
    UnknownSkill(String),
    /// SKILL.md がディスクから読み取れない。
    #[error("skill '{name}' の SKILL.md を読み込めません")]
    Unreadable { name: String },
    /// SKILL.md の frontmatter 構造が崩れている。
    #[error("skill '{name}' の SKILL.md は frontmatter 構造が不正です")]
    Corrupt { name: String },
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    /// 指定ディレクトリを指す repo スコープの entry を作る。
    fn entry(name: &str, dir: &Path) -> SkillEntry {
        SkillEntry {
            name: name.to_owned(),
            description: format!("{name} description"),
            dir: dir.to_path_buf(),
            scope: SkillScope::Repo,
        }
    }

    // -- available_skills -------------------------------------------------------

    /// Given: 名前順がバラバラに登録された skill 群
    /// When:  available_skills を繰り返し呼ぶ
    /// Then:  常に name 昇順かつ同一結果になる
    #[test]
    fn available_skills_is_name_sorted_and_deterministic() {
        let root = tempdir().unwrap();
        let registry = SkillRegistry {
            skills: BTreeMap::from([
                ("beta-skill".to_owned(), entry("beta-skill", root.path())),
                ("alpha-skill".to_owned(), entry("alpha-skill", root.path())),
                ("gamma-skill".to_owned(), entry("gamma-skill", root.path())),
            ]),
            diagnostics: Vec::new(),
        };

        let first = registry.available_skills();
        let second = registry.available_skills();

        let names: Vec<&str> = first.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, vec!["alpha-skill", "beta-skill", "gamma-skill"]);
        let descriptions: Vec<&str> = first
            .iter()
            .map(|skill| skill.description.as_str())
            .collect();
        assert_eq!(
            descriptions,
            vec![
                "alpha-skill description",
                "beta-skill description",
                "gamma-skill description"
            ]
        );
        assert_eq!(first, second);
    }

    // -- load_body --------------------------------------------------------------

    /// Given: frontmatter と本文、resources サブディレクトリを持つ skill
    /// When:  load_body を呼ぶ
    /// Then:  frontmatter を含まない本文だけが返る (再検証は行わない)
    #[test]
    fn load_body_returns_body_without_frontmatter() {
        let root = tempdir().unwrap();
        let skill_dir = root.path().join("demo-skill");
        fs::create_dir_all(skill_dir.join("resources")).unwrap();
        fs::write(skill_dir.join("resources").join("helper.txt"), "resource").unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: mismatched-name\ndescription: d\n---\nBody line.\nMore.\n",
        )
        .unwrap();
        let registry = SkillRegistry {
            skills: BTreeMap::from([("demo-skill".to_owned(), entry("demo-skill", &skill_dir))]),
            diagnostics: Vec::new(),
        };

        let body = registry.load_body("demo-skill").unwrap();

        assert_eq!(body, "Body line.\nMore.\n");
        assert!(!body.contains("name:"));
        assert!(!body.contains("---"));
    }

    // -- render_skills_section --------------------------------------------------

    // Given: (name, body) 2 件
    // When:  render_skills_section を呼ぶ
    // Then:  ヘッダに続き、要求順どおりのマーカー fence で本文が括られる
    #[test]
    fn render_skills_section_renders_header_and_fences_in_request_order() {
        let loaded = vec![
            ("alpha".to_owned(), "Alpha body.\n".to_owned()),
            ("beta".to_owned(), "Beta body.\n".to_owned()),
        ];

        let section = render_skills_section(&loaded);

        let expected = "## Skills\n\n\
            <!-- skill:alpha BEGIN -->\nAlpha body.\n\n<!-- skill:alpha END -->\n\n\
            <!-- skill:beta BEGIN -->\nBeta body.\n\n<!-- skill:beta END -->";
        assert_eq!(section, expected);
    }

    // Given: name 順と異なる要求順 (zulu → alpha)
    // When:  render_skills_section を呼ぶ
    // Then:  要求順を保持する (name ソートは行わない)
    #[test]
    fn render_skills_section_preserves_request_order_over_name_order() {
        let loaded = vec![
            ("zulu".to_owned(), "Z body.".to_owned()),
            ("alpha".to_owned(), "A body.".to_owned()),
        ];

        let section = render_skills_section(&loaded);

        let zulu = section.find("<!-- skill:zulu BEGIN -->").unwrap();
        let alpha = section.find("<!-- skill:alpha BEGIN -->").unwrap();
        assert!(zulu < alpha);
    }

    // Given: 空の load 済みリスト
    // When:  render_skills_section を繰り返し呼ぶ
    // Then:  ヘッダのみを返し、常にバイト一致する (決定的)
    #[test]
    fn render_skills_section_is_deterministic_and_header_only_for_empty_input() {
        assert_eq!(render_skills_section(&[]), "## Skills");
        let loaded = vec![("demo".to_owned(), "Body.".to_owned())];
        assert_eq!(
            render_skills_section(&loaded),
            render_skills_section(&loaded)
        );
    }

    /// Given: 未登録の skill 名
    /// When:  load_body を呼ぶ
    /// Then:  UnknownSkill で、エラー文言にその名前が載る
    #[test]
    fn load_body_unknown_skill_yields_unknown_skill_error() {
        let registry = SkillRegistry {
            skills: BTreeMap::new(),
            diagnostics: Vec::new(),
        };

        let err = registry.load_body("nope").unwrap_err();

        let message = err.to_string();
        assert!(matches!(err, SkillLoadError::UnknownSkill(name) if name == "nope"));
        assert!(message.contains("nope"));
    }

    /// Given: SKILL.md を登録後にディスクから削除した skill
    /// When:  load_body を呼ぶ
    /// Then:  Unreadable になる (登録時のメタデータで再読み込みに失敗)
    #[test]
    fn load_body_after_deleting_skill_md_yields_unreadable_error() {
        let root = tempdir().unwrap();
        let skill_dir = root.path().join("demo-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        fs::write(
            &skill_md,
            "---\nname: demo-skill\ndescription: d\n---\nBody.\n",
        )
        .unwrap();
        let registry = SkillRegistry {
            skills: BTreeMap::from([("demo-skill".to_owned(), entry("demo-skill", &skill_dir))]),
            diagnostics: Vec::new(),
        };

        fs::remove_file(&skill_md).unwrap();
        let err = registry.load_body("demo-skill").unwrap_err();

        assert!(matches!(err, SkillLoadError::Unreadable { name } if name == "demo-skill"));
    }
}
