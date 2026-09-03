//! プロジェクトルールの発見・選択・注入 API。

mod api;
mod budget;
mod discovery;
mod frontmatter;
mod matcher;
mod path;
mod render;
mod session;
mod source;
mod types;

pub use api::{after_successful_tools, startup_snapshot};
pub use session::RulesSession;
pub use source::RulesSource;
pub use types::{ProjectTrust, RulesError, RulesSettings};

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::{
        ProjectTrust, RulesSession, RulesSettings, RulesSource, after_successful_tools,
        startup_snapshot,
    };

    fn settings() -> RulesSettings {
        RulesSettings {
            context_window_tokens: 200_000,
            response_headroom_tokens: 16_384,
            max_injection_bytes: 65_536,
        }
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("親がある")).expect("ディレクトリを作れる");
        std::fs::write(path, content).expect("ファイルを書ける");
    }

    // Given: root・nested・scoped 規則 / When: startup snapshot / Then: root AGENTS だけを含む
    #[test]
    fn startup_excludes_nested_and_scoped_project_rules() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        write(&tmp.path().join("AGENTS.md"), "root-only");
        write(&tmp.path().join("src/AGENTS.md"), "nested-hidden");
        write(
            &tmp.path().join(".omo/rules/all.md"),
            "---\nalwaysApply: true\n---\nscoped-hidden",
        );
        let source = RulesSource::new(
            ProjectTrust::Approved,
            settings(),
            None,
            Some(tmp.path().to_path_buf()),
        );

        let snapshot =
            startup_snapshot(&source, Some(tmp.path()), None, 0).expect("root 規則がある");

        assert!(snapshot.contains("root-only"));
        assert!(!snapshot.contains("nested-hidden"));
        assert!(!snapshot.contains("scoped-hidden"));
    }

    // Given: user 規則と未承認 project 規則 / When: 両 entry point / Then: startup は user のみ、tool 後は None
    #[test]
    fn unapproved_project_is_never_injected() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let user = tmp.path().join("user");
        let project = tmp.path().join("project");
        write(
            &user.join("always.md"),
            "---\nalwaysApply: true\n---\nuser-visible",
        );
        write(&project.join("AGENTS.md"), "project-secret");
        let source = Arc::new(RulesSource::new(
            ProjectTrust::Unapproved,
            settings(),
            Some(user),
            Some(project.clone()),
        ));

        let startup = startup_snapshot(&source, Some(&project), None, 0).expect("user 規則がある");
        let mut session = RulesSession::new(Arc::clone(&source), Some(project.clone()));
        let after = after_successful_tools(&mut session, &[project.join("src/new.rs")]);

        assert!(startup.contains("user-visible"));
        assert!(!startup.contains("project-secret"));
        assert_eq!(after, None);
    }

    // Given: 2 対象で重なる AGENTS chain / When: tool 後 snapshot / Then: 共有 source は 1 回だけ現れる
    #[test]
    fn overlapping_targets_include_each_source_once() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        write(&tmp.path().join("AGENTS.md"), "shared-root-token");
        write(&tmp.path().join("a/AGENTS.md"), "a-token");
        write(&tmp.path().join("b/AGENTS.md"), "b-token");
        let source = Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            settings(),
            None,
            Some(tmp.path().to_path_buf()),
        ));
        let mut session = RulesSession::new(Arc::clone(&source), Some(tmp.path().to_path_buf()));

        let output = after_successful_tools(
            &mut session,
            &[tmp.path().join("a/x.rs"), tmp.path().join("b/y.rs")],
        )
        .expect("規則がある");

        assert_eq!(output.matches("shared-root-token").count(), 1);
        assert!(output.contains("a-token"));
        assert!(output.contains("b-token"));
    }

    // Given: 一度読み込んだ規則ファイル / When: 内容を変更して再度 tool 後 snapshot / Then: 新しい内容を読む
    #[test]
    fn tool_snapshot_rereads_modified_rule_file() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let rule = tmp.path().join("AGENTS.md");
        write(&rule, "before-token");
        let source = Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            settings(),
            None,
            Some(tmp.path().to_path_buf()),
        ));
        let mut session = RulesSession::new(source, Some(tmp.path().to_path_buf()));
        let targets = [tmp.path().join("src/new.rs")];
        let before = after_successful_tools(&mut session, &targets).expect("最初の規則");
        write(&rule, "after-token");

        let after = after_successful_tools(&mut session, &targets).expect("更新後の規則");

        assert!(before.contains("before-token"));
        assert!(after.contains("after-token"));
        assert!(!after.contains("before-token"));
    }

    // Given: alwaysApply と glob-only の user 規則 / When: startup snapshot / Then: alwaysApply だけを含む
    #[test]
    fn startup_includes_only_always_apply_user_rules() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        write(
            &tmp.path().join("always.md"),
            "---\nalwaysApply: true\n---\nalways-token",
        );
        write(
            &tmp.path().join("glob.md"),
            "---\nglobs: 'src/**'\n---\nglob-token",
        );
        let source = RulesSource::new(
            ProjectTrust::Unapproved,
            settings(),
            Some(tmp.path().to_path_buf()),
            None,
        );

        let output = startup_snapshot(&source, None, None, 0).expect("always 規則がある");

        assert!(output.contains("always-token"));
        assert!(!output.contains("glob-token"));
    }
}
