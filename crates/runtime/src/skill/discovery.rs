//! SKILL.md の発見とレジストリ構築 (issue #53 / AC3, AC7)。
//!
//! 発見はメタデータのみを行い、本文は読み込まない (progressive disclosure)。
//! 失敗は静かにしない (ADR 0010): 読めない SKILL.md やディレクトリは診断と
//! して報告する。ただし top-level の scope ディレクトリ自体が存在しない場合は
//! config と同じ許容で空として扱う。
//!
//! スコープ優先順位: 呼び出し側が与える `dirs` の順で処理し、先に現れた同名
//! 候補が勝つ (標準構成では repo → repo-agents → user → user-agents → builtin)。後に現れた同名候補は
//! `Shadowed` 診断付きで除外される。

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use event_bus::SkillDiagnosticKind;

use super::builtin::builtin_skill_entries;
use super::frontmatter::{parse_and_validate, read_frontmatter_prefix, split_frontmatter};
use super::registry::{SkillDiagnostic, SkillEntry, SkillRegistry, SkillScope, SkillSource};

/// 各スコープの skill ディレクトリを走査してレジストリを構築する。
///
/// `dirs` は優先度の高い順に与える (標準の fs は repo → repo-agents → user → user-agents)。各ディレクトリの
/// 直下サブディレクトリのうち `SKILL.md` を持つものが skill 候補で、
/// frontmatter を検証して登録する。検証に失敗した候補は `ValidationError`
/// 診断付きで除外され、同名の先発候補を持つ候補は `Shadowed` 診断付きで
/// 除外される。読み取れない SKILL.md やディレクトリは `DiscoveryError` 診断
/// として報告するが、scope ディレクトリ自体の欠損は許容して空として扱う。
pub fn discover_skills(dirs: &[(SkillScope, PathBuf)]) -> SkillRegistry {
    let mut registry = SkillRegistry::new(BTreeMap::new(), Vec::new());
    for (scope, dir) in dirs {
        let entries = scan_scope(*scope, dir, &mut registry.diagnostics);
        registry.merge_shadowing(entries);
    }
    registry
}

/// 明示した fs ディレクトリを探索し、最低優先度の builtin をマージする。
///
/// fs の優先順位・診断順序は [`discover_skills`] と同じ。同名の builtin は
/// `Shadowed` 診断と warn ログを残して除外する。
pub fn discover_with_builtin(dirs: &[(SkillScope, PathBuf)]) -> SkillRegistry {
    let mut registry = discover_skills(dirs);
    registry.merge_shadowing(builtin_skill_entries());
    registry
}

/// 標準探索: fs スコープ (repo .evorch → repo .agents → user config → HOME .agents) → builtin マージ。
pub fn build_standard_registry(repo_root: Option<&Path>) -> SkillRegistry {
    discover_with_builtin(&default_skill_dirs(repo_root))
}

/// 標準の skill ディレクトリ一覧を優先度順
/// (repo → repo-agents → user → user-agents) で返す。
///
/// 解決できないスコープはスキップする: `repo_root` が `None` なら repo と
/// repo-agents エントリなし、`config::user_config_dir` が `None` なら user エントリなし。
/// `$HOME` が未設定または空なら user-agents エントリなし。XDG から HOME は推測しない。
pub fn default_skill_dirs(repo_root: Option<&Path>) -> Vec<(SkillScope, PathBuf)> {
    let user_config = config::user_config_dir();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    default_skill_dirs_from(repo_root, user_config.as_deref(), home.as_deref())
}

fn default_skill_dirs_from(
    repo_root: Option<&Path>,
    user_config: Option<&Path>,
    home: Option<&Path>,
) -> Vec<(SkillScope, PathBuf)> {
    let mut dirs = Vec::new();
    if let Some(repo_root) = repo_root {
        dirs.push((SkillScope::Repo, repo_root.join(".evorch").join("skills")));
        dirs.push((
            SkillScope::RepoAgents,
            repo_root.join(".agents").join("skills"),
        ));
    }
    if let Some(user_dir) = user_config {
        dirs.push((SkillScope::User, user_dir.join("skills")));
    }
    if let Some(home) = home.filter(|home| !home.as_os_str().is_empty()) {
        dirs.push((SkillScope::UserAgents, home.join(".agents").join("skills")));
    }
    dirs
}

/// 1 スコープ分の走査。有効な entry を返し、診断は `diagnostics` に積む。
fn scan_scope(
    scope: SkillScope,
    dir: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Vec<SkillEntry> {
    let mut entries = Vec::new();
    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == ErrorKind::NotFound => return entries,
        Err(err) => {
            let detail = format!("skills directory is unreadable: {:?}", err.kind());
            diagnostics.push(discovery_error(scope, scope.as_str(), &detail));
            return entries;
        }
    };
    let mut subdirs = Vec::new();
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                let detail = format!("skills directory is unreadable: {:?}", err.kind());
                diagnostics.push(discovery_error(scope, scope.as_str(), &detail));
                continue;
            }
        };
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => subdirs.push(entry.path()),
            Ok(_) => {}
            Err(err) => {
                let name = entry.file_name().to_string_lossy().into_owned();
                let detail = format!("cannot inspect skill directory '{name}': {:?}", err.kind());
                diagnostics.push(discovery_error(scope, &name, &detail));
            }
        }
    }
    // read_dir の走査順は OS 依存のため、診断の決定性を保証する目的でソートする。
    subdirs.sort();
    for subdir in subdirs {
        match scan_candidate(scope, &subdir) {
            CandidateOutcome::Skip => {}
            CandidateOutcome::Admit(entry) => entries.push(entry),
            CandidateOutcome::Reject(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    entries
}

/// 1 候補ディレクトリの処理結果。
enum CandidateOutcome {
    /// SKILL.md を持たないため候補外 (診断なしで無視)。
    Skip,
    /// 検証に成功した skill として登録する。
    Admit(SkillEntry),
    /// 除外し、診断を記録する。
    Reject(SkillDiagnostic),
}

/// 1 候補ディレクトリを評価する。SKILL.md がなければ [`CandidateOutcome::Skip`]、
/// 読めなければ `DiscoveryError`、検証できなければ `ValidationError` の診断を
/// [`CandidateOutcome::Reject`] で返す。
fn scan_candidate(scope: SkillScope, dir: &Path) -> CandidateOutcome {
    let dir_name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| scope.as_str().to_owned());
    let content = match read_frontmatter_prefix(&dir.join("SKILL.md")) {
        Ok(content) => content,
        Err(err) if err.kind() == ErrorKind::NotFound => return CandidateOutcome::Skip,
        Err(err) => {
            let detail = format!(
                "SKILL.md in skill directory '{dir_name}' is unreadable: {:?}",
                err.kind()
            );
            return CandidateOutcome::Reject(discovery_error(scope, &dir_name, &detail));
        }
    };
    match parse_and_validate(&content, &dir_name) {
        Ok(frontmatter) => CandidateOutcome::Admit(SkillEntry {
            name: frontmatter.name,
            description: frontmatter.description,
            source: SkillSource::Filesystem {
                dir: dir.to_path_buf(),
            },
            scope,
        }),
        Err(err) => {
            let detail = err.to_string();
            let skill = recover_name(&content).unwrap_or(dir_name);
            tracing::warn!(
                skill = %skill,
                scope = scope.as_str(),
                reason = %detail,
                "invalid SKILL.md; skill excluded from registry"
            );
            CandidateOutcome::Reject(SkillDiagnostic {
                kind: SkillDiagnosticKind::ValidationError,
                skill,
                scope,
                detail,
            })
        }
    }
}

/// 検証に失敗した内容から frontmatter の name をベストエフォートで復元する。
///
/// フェンス構造も YAML も壊れている場合は `None` を返し、呼び出し側は
/// ディレクトリ名へフォールバックする。
fn recover_name(content: &str) -> Option<String> {
    let (yaml, _) = split_frontmatter(content).ok()?;
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).ok()?;
    value.get("name")?.as_str().map(str::to_owned)
}

/// `DiscoveryError` 診断を構築し、warn ログにも出す。
fn discovery_error(scope: SkillScope, skill: &str, detail: &str) -> SkillDiagnostic {
    tracing::warn!(
        skill = %skill,
        scope = scope.as_str(),
        reason = %detail,
        "skill discovery error"
    );
    SkillDiagnostic {
        kind: SkillDiagnosticKind::DiscoveryError,
        skill: skill.to_owned(),
        scope,
        detail: detail.to_owned(),
    }
}
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    /// name/description/body から SKILL.md を持つ skill ディレクトリを作る。
    fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
        )
        .unwrap();
    }

    // -- discover_skills: 正常系 ----------------------------------------------

    /// Given: repo スコープに有効な skill が 1 つある
    /// When:  discover_skills を呼ぶ
    /// Then:  frontmatter 由来のメタデータで entry が登録される
    #[test]
    fn discovers_repo_scope_skill_with_metadata() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        write_skill(&skills, "demo-skill", "Demo skill", "Body.\n");

        let registry = discover_skills(&[(SkillScope::Repo, skills.clone())]);

        assert_eq!(registry.len(), 1);
        let entry = registry.get("demo-skill").unwrap();
        assert_eq!(entry.name, "demo-skill");
        assert_eq!(entry.description, "Demo skill");
        assert_eq!(entry.scope, SkillScope::Repo);
        assert_eq!(
            entry.source,
            SkillSource::Filesystem {
                dir: skills.join("demo-skill"),
            }
        );
    }

    /// Given: user スコープに有効な skill が 1 つある
    /// When:  discover_skills を呼ぶ
    /// Then:  scope User で entry が登録される
    #[test]
    fn discovers_user_scope_skill() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        write_skill(&skills, "user-skill", "User skill", "Body.\n");

        let registry = discover_skills(&[(SkillScope::User, skills)]);

        assert_eq!(registry.len(), 1);
        let entry = registry.get("user-skill").unwrap();
        assert_eq!(entry.scope, SkillScope::User);
    }

    /// Given: SKILL.md を持たないサブディレクトリとファイルが混在する
    /// When:  discover_skills を呼ぶ
    /// Then:  候補外は診断なしで無視される
    #[test]
    fn directories_without_skill_md_are_ignored() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        fs::create_dir_all(skills.join("no-skill-md")).unwrap();
        fs::write(skills.join("plain-file.txt"), "not a skill").unwrap();
        write_skill(&skills, "demo-skill", "Demo skill", "Body.\n");

        let registry = discover_skills(&[(SkillScope::Repo, skills)]);

        assert_eq!(registry.len(), 1);
        assert!(registry.diagnostics.is_empty());
    }

    // -- discover_skills: スコープ優先 (AC3) ------------------------------------

    /// Given: 同名 skill が repo と user の両方に存在する
    /// When:  repo → user の順で discover_skills を呼ぶ
    /// Then:  repo 側の entry が採用され、Shadowed 診断が 1 件だけ出る
    #[test]
    fn repo_scope_shadows_user_scope_and_emits_shadowed_diagnostic() {
        let repo_root = tempdir().unwrap();
        let user_root = tempdir().unwrap();
        let repo_skills = repo_root.path().join("skills");
        let user_skills = user_root.path().join("skills");
        write_skill(&repo_skills, "demo-skill", "Repo version", "Repo body.\n");
        write_skill(&user_skills, "demo-skill", "User version", "User body.\n");

        let registry = discover_skills(&[
            (SkillScope::Repo, repo_skills),
            (SkillScope::User, user_skills),
        ]);

        assert_eq!(registry.len(), 1);
        let entry = registry.get("demo-skill").unwrap();
        assert_eq!(entry.scope, SkillScope::Repo);
        assert_eq!(entry.description, "Repo version");

        assert_eq!(registry.diagnostics.len(), 1);
        let diagnostic = &registry.diagnostics[0];
        assert_eq!(diagnostic.kind, SkillDiagnosticKind::Shadowed);
        assert_eq!(diagnostic.skill, "demo-skill");
        assert_eq!(diagnostic.scope, SkillScope::User);
        assert!(diagnostic.detail.contains("repo"));
        assert!(diagnostic.detail.contains("user"));
        assert!(diagnostic.detail.contains("demo-skill"));
    }

    /// Given: .agents/skills 直下に有効な skill がある
    /// When:  標準レジストリを構築する
    /// Then:  RepoAgents として発見され、fs 本文・リソースを読める
    #[test]
    fn standard_registry_discovers_repo_agents_skill() {
        let root = tempdir().unwrap();
        let skills = root.path().join(".agents").join("skills");
        write_skill(
            &skills,
            "agents-discovery-fixture",
            "Agents skill",
            "Agents body.\n",
        );
        fs::write(
            skills.join("agents-discovery-fixture/NOTES.md"),
            "Agent note.\n",
        )
        .unwrap();

        let registry = build_standard_registry(Some(root.path()));

        let entry = registry.get("agents-discovery-fixture").unwrap();
        assert_eq!(entry.scope, SkillScope::RepoAgents);
        assert_eq!(entry.description, "Agents skill");
        assert_eq!(
            entry.source,
            SkillSource::Filesystem {
                dir: skills.join("agents-discovery-fixture")
            }
        );
        assert_eq!(
            registry.load_body("agents-discovery-fixture").unwrap(),
            "Agents body.\n"
        );
        assert_eq!(entry.read_resource("NOTES.md").unwrap(), "Agent note.\n");
    }

    /// 標準ディレクトリ順を使い、user の実環境に依存せず repo 間の優先度を確認する。
    #[test]
    fn repo_scope_shadows_repo_agents_scope() {
        let root = tempdir().unwrap();
        let repo = root.path().join(".evorch/skills");
        let agents = root.path().join(".agents/skills");
        write_skill(&repo, "demo-skill", "Repo winner", "Repo body.\n");
        write_skill(&agents, "demo-skill", "Agents loser", "Agents body.\n");
        let dirs = default_skill_dirs(Some(root.path()));

        let registry = discover_skills(&dirs[..2]);

        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("demo-skill").unwrap().scope, SkillScope::Repo);
        assert_eq!(registry.load_body("demo-skill").unwrap(), "Repo body.\n");
        assert_eq!(registry.diagnostics.len(), 1);
        let diagnostic = &registry.diagnostics[0];
        assert_eq!(diagnostic.kind, SkillDiagnosticKind::Shadowed);
        assert_eq!(diagnostic.skill, "demo-skill");
        assert_eq!(diagnostic.scope, SkillScope::RepoAgents);
        assert_eq!(
            diagnostic.detail,
            "skill 'demo-skill': repo scope shadows repo-agents scope"
        );
    }

    #[test]
    fn repo_agents_scope_shadows_user_scope() {
        let root = tempdir().unwrap();
        let agents = root.path().join(".agents/skills");
        let user = root.path().join("user/skills");
        write_skill(&agents, "demo-skill", "Agents winner", "Agents body.\n");
        write_skill(&user, "demo-skill", "User loser", "User body.\n");

        let registry =
            discover_skills(&[(SkillScope::RepoAgents, agents), (SkillScope::User, user)]);

        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.get("demo-skill").unwrap().scope,
            SkillScope::RepoAgents
        );
        assert_eq!(registry.load_body("demo-skill").unwrap(), "Agents body.\n");
        assert_eq!(registry.diagnostics.len(), 1);
        let diagnostic = &registry.diagnostics[0];
        assert_eq!(diagnostic.kind, SkillDiagnosticKind::Shadowed);
        assert_eq!(diagnostic.skill, "demo-skill");
        assert_eq!(diagnostic.scope, SkillScope::User);
        assert_eq!(
            diagnostic.detail,
            "skill 'demo-skill': repo-agents scope shadows user scope"
        );
    }

    #[test]
    fn discovery_scans_only_immediate_scope_children() {
        let root = tempdir().unwrap();
        let skills = root.path().join(".agents/skills");
        write_skill(
            &skills.join("nested"),
            "hidden-skill",
            "Not discovered",
            "Hidden body.\n",
        );
        write_skill(&skills, "visible-skill", "Discovered", "Visible body.\n");

        let registry = discover_skills(&[(SkillScope::RepoAgents, skills)]);

        assert_eq!(registry.len(), 1);
        assert!(registry.get("hidden-skill").is_none());
        assert!(registry.get("visible-skill").is_some());
        assert!(registry.diagnostics.is_empty());
    }

    #[test]
    fn discovery_uses_caller_order_instead_of_scope_ranking() {
        let root = tempdir().unwrap();
        let repo = root.path().join(".evorch/skills");
        let user = root.path().join("user/skills");
        write_skill(&repo, "demo-skill", "Repo loser", "Repo body.\n");
        write_skill(&user, "demo-skill", "User winner", "User body.\n");

        let registry = discover_skills(&[(SkillScope::User, user), (SkillScope::Repo, repo)]);

        assert_eq!(registry.get("demo-skill").unwrap().scope, SkillScope::User);
        assert_eq!(registry.load_body("demo-skill").unwrap(), "User body.\n");
        assert_eq!(registry.diagnostics.len(), 1);
        assert_eq!(registry.diagnostics[0].scope, SkillScope::Repo);
        assert_eq!(registry.diagnostics[0].kind, SkillDiagnosticKind::Shadowed);
    }

    // -- discover_skills: 無効 skill の除外 (AC7) --------------------------------

    /// Given: name がディレクトリ名と一致しない SKILL.md を持つ skill
    /// When:  discover_skills を呼ぶ
    /// Then:  available_skills には現れず、ValidationError 診断が 1 件出る
    #[test]
    fn invalid_skill_is_excluded_with_validation_diagnostic() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        let skill_dir = skills.join("demo-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: other-name\ndescription: Broken skill\n---\nBody.\n",
        )
        .unwrap();

        let registry = discover_skills(&[(SkillScope::Repo, skills)]);

        assert!(registry.is_empty());
        assert!(registry.available_skills().is_empty());
        assert_eq!(registry.diagnostics.len(), 1);
        let diagnostic = &registry.diagnostics[0];
        assert_eq!(diagnostic.kind, SkillDiagnosticKind::ValidationError);
        assert_eq!(diagnostic.skill, "other-name");
        assert_eq!(diagnostic.scope, SkillScope::Repo);
        assert!(!diagnostic.detail.is_empty());
    }

    /// Given: SKILL.md が不正な UTF-8 バイト列で読み取れない
    /// When:  discover_skills を呼ぶ
    /// Then:  DiscoveryError 診断が 1 件出る (静かに無視しない)
    #[test]
    fn unreadable_skill_md_yields_discovery_error() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        let skill_dir = skills.join("demo-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), [0xff, 0xfe, 0x00]).unwrap();

        let registry = discover_skills(&[(SkillScope::Repo, skills)]);

        assert!(registry.is_empty());
        assert_eq!(registry.diagnostics.len(), 1);
        let diagnostic = &registry.diagnostics[0];
        assert_eq!(diagnostic.kind, SkillDiagnosticKind::DiscoveryError);
        assert_eq!(diagnostic.skill, "demo-skill");
        assert_eq!(diagnostic.scope, SkillScope::Repo);
    }

    #[test]
    fn unreadable_scope_diagnostic_contains_identifiers_not_full_paths() {
        let root = tempdir().unwrap();
        let skills = root.path().join("not-a-directory");
        fs::write(&skills, "PRIVATE CONTENT SENTINEL").unwrap();

        let registry = discover_skills(&[(SkillScope::RepoAgents, skills)]);

        assert!(registry.is_empty());
        assert_eq!(registry.diagnostics.len(), 1);
        let diagnostic = &registry.diagnostics[0];
        assert_eq!(diagnostic.kind, SkillDiagnosticKind::DiscoveryError);
        assert_eq!(diagnostic.scope, SkillScope::RepoAgents);
        assert_eq!(diagnostic.skill, "repo-agents");
        assert!(diagnostic.detail.contains("unreadable"));
        assert!(!diagnostic.detail.contains(root.path().to_str().unwrap()));
        assert!(!diagnostic.detail.contains("SENTINEL"));
    }

    // -- discover_skills: 本文の非実体化 (AC4) ------------------------------------

    /// Given: frontmatter は妥当だが本文が不正な UTF-8 バイト列である SKILL.md
    /// When:  discover_skills を呼ぶ
    /// Then:  本文を実体化せず frontmatter 先頭部分のみを読むため skill が
    ///        発見され、frontmatter 由来のメタデータが登録される
    #[test]
    fn discovery_does_not_materialize_skill_body() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        let skill_dir = skills.join("demo-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        let mut content = b"---\nname: demo-skill\ndescription: Demo skill\n---\n".to_vec();
        content.extend([0xff_u8, 0xfe].repeat(4096));
        fs::write(skill_dir.join("SKILL.md"), content).unwrap();

        let registry = discover_skills(&[(SkillScope::Repo, skills)]);

        assert_eq!(registry.len(), 1);
        let entry = registry.get("demo-skill").unwrap();
        assert_eq!(entry.name, "demo-skill");
        assert_eq!(entry.description, "Demo skill");
        let available = registry.available_skills();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].name, "demo-skill");
        assert_eq!(available[0].description, "Demo skill");
    }

    /// Given: 本文にマルチバイト UTF-8 文字を含む SKILL.md
    /// When:  discover_skills を呼ぶ
    /// Then:  本文の文字種によらず frontmatter 由来のメタデータで発見される
    #[test]
    fn discovers_skill_with_multibyte_utf8_body() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        write_skill(
            &skills,
            "demo-skill",
            "Demo skill",
            "本文はロードされない。\n",
        );

        let registry = discover_skills(&[(SkillScope::Repo, skills)]);

        assert_eq!(registry.len(), 1);
        let entry = registry.get("demo-skill").unwrap();
        assert_eq!(entry.name, "demo-skill");
        assert_eq!(entry.description, "Demo skill");
    }

    /// Given: 閉じフェンスを欠く SKILL.md (内容は UTF-8 として有効)
    /// When:  discover_skills を呼ぶ
    /// Then:  ValidationError 診断が 1 件出る (frontmatter 規約は読み取り方式に依存しない)
    #[test]
    fn missing_closing_fence_yields_validation_diagnostic() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        let skill_dir = skills.join("demo-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: Fenceless skill\n",
        )
        .unwrap();

        let registry = discover_skills(&[(SkillScope::Repo, skills)]);

        assert!(registry.is_empty());
        assert_eq!(registry.diagnostics.len(), 1);
        let diagnostic = &registry.diagnostics[0];
        assert_eq!(diagnostic.kind, SkillDiagnosticKind::ValidationError);
        assert_eq!(diagnostic.skill, "demo-skill");
    }

    // -- discover_skills: scope ディレクトリの欠損 -------------------------------

    /// Given: 標準の全 scope ディレクトリが存在しない
    /// When:  discover_skills を呼ぶ
    /// Then:  空レジストリかつ診断ゼロ (config と同じ許容)
    #[test]
    fn missing_scope_dirs_yield_empty_registry_without_diagnostics() {
        let root = tempdir().unwrap();
        let dirs = default_skill_dirs_from(Some(root.path()), Some(root.path()), Some(root.path()));
        let registry = discover_skills(&dirs);

        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.diagnostics.is_empty());
    }

    #[test]
    fn discover_with_builtin_uses_explicit_dirs_without_user_config() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        write_skill(
            &skills,
            "explicit-skill",
            "Explicit fixture",
            "Explicit body.\n",
        );

        let registry = discover_with_builtin(&[(SkillScope::RepoAgents, skills)]);

        let builtins = discover_with_builtin(&[]);
        assert_eq!(registry.len(), builtins.len() + 1);
        assert_eq!(
            registry.get("explicit-skill").unwrap().scope,
            SkillScope::RepoAgents
        );
        assert_eq!(
            registry.load_body("explicit-skill").unwrap(),
            "Explicit body.\n"
        );
        assert!(registry.diagnostics.is_empty());
        // 明示 dirs が空なら builtin のみで user config は探索しない。
        assert!(!builtins.is_empty());
        assert!(
            builtins.available_skills().iter().all(|skill| builtins
                .get(&skill.name)
                .unwrap()
                .scope
                == SkillScope::Builtin)
        );
        assert!(builtins.diagnostics.is_empty());
    }

    // -- default_skill_dirs ------------------------------------------------------

    #[test]
    fn default_skill_dirs_lists_all_scopes_in_priority_order() {
        let repo = Path::new("/repo");
        let config = Path::new("/xdg/evorch");
        let home = Path::new("/home/fixture");

        assert_eq!(
            default_skill_dirs_from(Some(repo), Some(config), Some(home)),
            [
                (SkillScope::Repo, repo.join(".evorch/skills")),
                (SkillScope::RepoAgents, repo.join(".agents/skills")),
                (SkillScope::User, config.join("skills")),
                (SkillScope::UserAgents, home.join(".agents/skills")),
            ]
        );
    }

    #[test]
    fn default_skill_dirs_skips_unresolved_scopes_without_inferring_home_from_xdg() {
        let config = Path::new("/xdg/evorch");
        let home = Path::new("/home/fixture");
        let user = vec![(SkillScope::User, config.join("skills"))];

        assert_eq!(default_skill_dirs_from(None, Some(config), None), user);
        assert_eq!(
            default_skill_dirs_from(None, Some(config), Some(Path::new(""))),
            user
        );
        assert_eq!(
            default_skill_dirs_from(None, None, Some(home)),
            [(SkillScope::UserAgents, home.join(".agents/skills"))]
        );
        assert!(default_skill_dirs_from(None, None, None).is_empty());
    }

    #[test]
    fn discovers_home_agents_skill_with_body_resource_and_scope_diagnostics() {
        let root = tempdir().unwrap();
        let skills = root.path().join(".agents/skills");
        write_skill(&skills, "home-skill", "Home skill", "Home body.\n");
        fs::write(skills.join("home-skill/NOTES.md"), "Home note.\n").unwrap();
        write_skill(&skills, "broken-skill", "", "Broken body.\n");

        let registry = discover_skills(&default_skill_dirs_from(None, None, Some(root.path())));

        assert_eq!(registry.len(), 1);
        let entry = registry.get("home-skill").unwrap();
        assert_eq!(entry.scope, SkillScope::UserAgents);
        assert_eq!(entry.scope.as_str(), "user-agents");
        assert_eq!(entry.description, "Home skill");
        assert_eq!(
            entry.source,
            SkillSource::Filesystem {
                dir: skills.join("home-skill")
            }
        );
        assert_eq!(registry.load_body("home-skill").unwrap(), "Home body.\n");
        assert_eq!(entry.read_resource("NOTES.md").unwrap(), "Home note.\n");
        assert_eq!(registry.diagnostics.len(), 1);
        let diagnostic = &registry.diagnostics[0];
        assert_eq!(diagnostic.scope, SkillScope::UserAgents);
        assert_eq!(diagnostic.kind, SkillDiagnosticKind::ValidationError);
        assert_eq!(diagnostic.skill, "broken-skill");
    }

    #[test]
    fn standard_scope_priority_shadows_home_agents_and_reports_each_loser() {
        let root = tempdir().unwrap();
        let repo = root.path().join("repo");
        let user_config = root.path().join("xdg/evorch");
        let home = root.path().join("home");
        let dirs = default_skill_dirs_from(Some(&repo), Some(&user_config), Some(&home));
        for (scope, dir) in &dirs {
            write_skill(dir, "shared-skill", scope.as_str(), scope.as_str());
        }

        // Remove each winner in turn to exercise every fallback through the standard order.
        for (index, (scope, dir)) in dirs.iter().enumerate() {
            let registry = discover_skills(&dirs);
            assert_eq!(registry.len(), 1);
            assert_eq!(registry.get("shared-skill").unwrap().scope, *scope);
            assert_eq!(registry.load_body("shared-skill").unwrap(), scope.as_str());
            assert_eq!(registry.diagnostics.len(), dirs.len() - index - 1);
            for (diagnostic, (loser, _)) in registry.diagnostics.iter().zip(&dirs[index + 1..]) {
                assert_eq!(diagnostic.kind, SkillDiagnosticKind::Shadowed);
                assert_eq!(diagnostic.scope, *loser);
                assert_eq!(
                    diagnostic.detail,
                    format!(
                        "skill 'shared-skill': {} scope shadows {} scope",
                        scope.as_str(),
                        loser.as_str()
                    )
                );
            }
            fs::remove_dir_all(dir.join("shared-skill")).unwrap();
        }
    }

    #[test]
    fn unreadable_home_agents_root_reports_user_agents_discovery_error() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(".agents")).unwrap();
        fs::write(root.path().join(".agents/skills"), "not a directory").unwrap();

        let registry = discover_skills(&default_skill_dirs_from(None, None, Some(root.path())));

        assert!(registry.is_empty());
        assert_eq!(registry.diagnostics.len(), 1);
        assert_eq!(registry.diagnostics[0].scope, SkillScope::UserAgents);
        assert_eq!(registry.diagnostics[0].skill, "user-agents");
        assert_eq!(
            registry.diagnostics[0].kind,
            SkillDiagnosticKind::DiscoveryError
        );
    }
}
