//! SKILL.md メタデータレジストリ (issue #53 / AC3, AC7)。
//!
//! レジストリは frontmatter 由来のメタデータと読み込み元を保持する。fs の本文は
//! [`SkillRegistry::load_body`] で都度ディスクから読み直し、同梱本文は静的データを
//! 返す (progressive disclosure)。エラー Display は識別子のみを運び、本文や
//! frontmatter 値・フルパスを漏らさない (ADR 0010 / frontmatter と同一規約)。

use std::collections::BTreeMap;
use std::path::PathBuf;

use event_bus::SkillDiagnosticKind;

use super::frontmatter::split_frontmatter;
use super::resource::{SkillResourceError, read_skill_resource, validate_reference};
use crate::prompt::AvailableSkill;

/// skill の由来スコープ。標準優先順位は repo > repo-agents > user > user-agents > builtin。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    /// リポジトリスコープ (`<repo>/.evorch/skills`)。
    Repo,
    /// エージェント共用のリポジトリスコープ (`<repo>/.agents/skills`)。
    RepoAgents,
    /// ユーザスコープ (`<user config>/evorch/skills`)。
    User,
    /// エージェント共用のユーザスコープ (`$HOME/.agents/skills`)。
    UserAgents,
    /// バイナリ同梱スコープ。
    Builtin,
}

impl SkillScope {
    /// 診断 detail やイベントペイロード用の固定識別子。
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillScope::Repo => "repo",
            SkillScope::RepoAgents => "repo-agents",
            SkillScope::User => "user",
            SkillScope::UserAgents => "user-agents",
            SkillScope::Builtin => "builtin",
        }
    }

    /// リポジトリスコープの探索ディレクトリをリポジトリルート相対で返す。
    /// エラー文言用で、ユーザ・builtin スコープは `None`。
    pub(crate) fn repo_dir(&self) -> Option<&'static str> {
        match self {
            SkillScope::Repo => Some(".evorch/skills"),
            SkillScope::RepoAgents => Some(".agents/skills"),
            SkillScope::User | SkillScope::UserAgents | SkillScope::Builtin => None,
        }
    }
}

/// skill 本文・リソースの読み込み元。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// SKILL.md を含む skill ディレクトリ。
    Filesystem { dir: PathBuf },
    /// バイナリに同梱された本文と、相対リファレンス・内容の組。
    Embedded {
        /// frontmatter を除いた本文のみ。
        body: &'static str,
        /// リファレンスは `file` または `dir/file` の形状規約に従う。
        resources: &'static [(&'static str, &'static str)],
    },
}

/// 発見済み skill のメタデータと読み込み元 1 件分。fs 本文は保持しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    /// skill 名。fs 由来なら frontmatter の `name` とディレクトリ名は一致検証済み。
    pub name: String,
    /// skill の説明。fs 由来なら frontmatter の `description`。
    pub description: String,
    /// 本文・リソースの読み込み元。
    pub source: SkillSource,
    /// この entry の由来スコープ。
    pub scope: SkillScope,
}

impl SkillEntry {
    /// skill ルート相対のバンドルリソースを読み出す。
    ///
    /// fs 由来は [`read_skill_resource`] で形状検証と symlink 脱出防止を行う。
    /// 同梱リソースは同じ形状規約を検証後、静的テーブルから検索する。
    ///
    /// # Errors
    /// 形状規約違反は [`SkillResourceError::InvalidReference`]、未登録リソースは
    /// [`SkillResourceError::NotFound`]。fs 由来では UTF-8 違反や symlink 脱出も
    /// [`SkillResourceError`] として返す。
    pub fn read_resource(&self, reference: &str) -> Result<String, SkillResourceError> {
        match &self.source {
            SkillSource::Filesystem { dir } => read_skill_resource(dir, reference),
            SkillSource::Embedded { resources, .. } => {
                validate_reference(reference)
                    .map_err(|reason| SkillResourceError::InvalidReference(reason.to_owned()))?;
                resources
                    .iter()
                    .find(|(name, _)| *name == reference)
                    .map(|(_, content)| (*content).to_owned())
                    .ok_or_else(|| SkillResourceError::NotFound(reference.to_owned()))
            }
        }
    }
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
    /// プロジェクトが未信頼のため探索しなかったリポジトリ skill の名前と由来スコープ。
    /// 名前はディレクトリ名だけから得ており、SKILL.md の内容は読んでいない。
    /// 未登録名のエラーで原因を伝えるためだけに使い、catalog には現れない。
    untrusted: BTreeMap<String, SkillScope>,
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
            untrusted: BTreeMap::new(),
        }
    }

    /// 未信頼プロジェクトのため除外したリポジトリ skill の名前を記録する。
    /// 登録済み skill・catalog・診断は変えない。
    pub(crate) fn withhold_untrusted(&mut self, withheld: BTreeMap<String, SkillScope>) {
        self.untrusted = withheld;
    }

    /// 未登録の `name` がプロジェクトの未信頼で除外されたものなら、その理由を返す。
    pub fn untrusted_reason(&self, name: &str) -> Option<SkillLoadError> {
        let dir = self.untrusted.get(name)?.repo_dir()?;
        Some(SkillLoadError::UntrustedProject {
            name: name.to_owned(),
            dir,
        })
    }

    /// 登録されていない `name` を拒否するときのツールエラー文言。
    ///
    /// 未信頼プロジェクトで除外された名前なら原因と対処を、それ以外は従来どおり
    /// `unknown skill: <name>` を返す。
    pub fn unknown_skill_message(&self, name: &str) -> String {
        self.untrusted_reason(name).map_or_else(
            || format!("unknown skill: {name}"),
            |reason| reason.to_string(),
        )
    }

    /// 後続候補を first-wins でマージする。診断は既存の末尾に追加する。
    pub(crate) fn merge_shadowing(&mut self, entries: Vec<SkillEntry>) {
        for entry in entries {
            if let Some(winner) = self.skills.get(&entry.name) {
                let detail = format!(
                    "skill '{}': {} scope shadows {} scope",
                    entry.name,
                    winner.scope.as_str(),
                    entry.scope.as_str()
                );
                tracing::warn!(
                    skill = %entry.name,
                    winner_scope = winner.scope.as_str(),
                    loser_scope = entry.scope.as_str(),
                    "skill shadowed by higher-priority scope"
                );
                self.diagnostics.push(SkillDiagnostic {
                    kind: SkillDiagnosticKind::Shadowed,
                    skill: entry.name,
                    scope: entry.scope,
                    detail,
                });
            } else {
                self.skills.insert(entry.name.clone(), entry);
            }
        }
    }

    /// 登録済み entry を name 昇順で返す。
    pub(crate) fn entries(&self) -> impl Iterator<Item = &SkillEntry> {
        self.skills.values()
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

    /// skill 本文 (frontmatter 直後から末尾まで) を読み込み元から返す。
    ///
    /// progressive disclosure のため fs 本文は発見時に保持せず、この場で
    /// `<dir>/SKILL.md` を読む。発見時の検証は繰り返さない (frontmatter の
    /// name とディレクトリ名の一致は再確認しない)。同梱本文はそのまま返す。
    ///
    /// # Errors
    /// 未登録名なら [`SkillLoadError::UnknownSkill`] (未信頼プロジェクトで除外した
    /// 名前なら [`SkillLoadError::UntrustedProject`])、SKILL.md が読めなければ
    /// [`SkillLoadError::Unreadable`]、frontmatter 構造の分割に失敗すれば
    /// [`SkillLoadError::Corrupt`]。
    pub fn load_body(&self, name: &str) -> Result<String, SkillLoadError> {
        let Some(entry) = self.skills.get(name) else {
            return Err(self
                .untrusted_reason(name)
                .unwrap_or_else(|| SkillLoadError::UnknownSkill(name.to_owned())));
        };
        match &entry.source {
            SkillSource::Filesystem { dir } => {
                let content = std::fs::read_to_string(dir.join("SKILL.md")).map_err(|_| {
                    SkillLoadError::Unreadable {
                        name: entry.name.clone(),
                    }
                })?;
                let (_, body) =
                    split_frontmatter(&content).map_err(|_| SkillLoadError::Corrupt {
                        name: entry.name.clone(),
                    })?;
                Ok(body.to_owned())
            }
            SkillSource::Embedded { body, .. } => Ok((*body).to_owned()),
        }
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
    /// プロジェクトが未信頼のため、リポジトリ skill を探索から除外している。
    /// モデルとユーザに原因と対処を伝えるため、ツールエラーと同じ英語で書く。
    #[error(
        "skill '{name}' exists in the project's {dir} but project skills are disabled because the project is not trusted; ask the user to enable \"Trust project instructions\" in Project settings"
    )]
    UntrustedProject { name: String, dir: &'static str },
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
            source: SkillSource::Filesystem {
                dir: dir.to_path_buf(),
            },
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
            untrusted: BTreeMap::new(),
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
            untrusted: BTreeMap::new(),
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
            untrusted: BTreeMap::new(),
        };

        let err = registry.load_body("nope").unwrap_err();

        let message = err.to_string();
        assert!(matches!(err, SkillLoadError::UnknownSkill(name) if name == "nope"));
        assert!(message.contains("nope"));
    }

    /// Given: 未信頼プロジェクトのため repo / repo-agents で除外した名前を持つレジストリ
    /// When:  load_body と unknown_skill_message を呼ぶ
    /// Then:  除外した名前は原因と探索ディレクトリ・対処を返し、それ以外は unknown のまま
    #[test]
    fn withheld_untrusted_names_explain_the_trust_gate() {
        let mut registry = SkillRegistry::new(BTreeMap::new(), Vec::new());
        registry.withhold_untrusted(BTreeMap::from([
            ("evorch-skill".to_owned(), SkillScope::Repo),
            ("agents-skill".to_owned(), SkillScope::RepoAgents),
        ]));

        let err = registry.load_body("agents-skill").unwrap_err();

        assert!(matches!(
            &err,
            SkillLoadError::UntrustedProject { name, dir }
                if name == "agents-skill" && *dir == ".agents/skills"
        ));
        assert_eq!(
            registry.unknown_skill_message("evorch-skill"),
            "skill 'evorch-skill' exists in the project's .evorch/skills but project skills are \
             disabled because the project is not trusted; ask the user to enable \
             \"Trust project instructions\" in Project settings"
        );
        assert_eq!(
            err.to_string(),
            registry.unknown_skill_message("agents-skill")
        );
        assert!(matches!(
            registry.load_body("nope").unwrap_err(),
            SkillLoadError::UnknownSkill(name) if name == "nope"
        ));
        assert_eq!(
            registry.unknown_skill_message("nope"),
            "unknown skill: nope"
        );
        assert!(registry.is_empty());
        assert!(registry.available_skills().is_empty());
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
            untrusted: BTreeMap::new(),
        };

        fs::remove_file(&skill_md).unwrap();
        let err = registry.load_body("demo-skill").unwrap_err();

        assert!(matches!(err, SkillLoadError::Unreadable { name } if name == "demo-skill"));
    }
}
