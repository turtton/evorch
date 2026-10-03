use super::*;

fn index(paths: &[&str], skills: &[(&str, &str)]) -> MentionIndex {
    let files: Vec<_> = paths.iter().map(|path| (*path).to_owned()).collect();
    MentionIndex::from_parts(
        None,
        &files,
        skills
            .iter()
            .map(|(name, description)| ((*name).to_owned(), (*description).to_owned())),
    )
}

fn labels(index: &MentionIndex, input: &str) -> Vec<String> {
    let mention = mention_at(input, input.len()).expect("mention");
    index
        .complete(&mention)
        .into_iter()
        .map(|item| item.label)
        .collect()
}

fn skill_registry(root: &Path, skills: &[(&str, &str)]) -> SkillRegistry {
    let dir = root.join(".evorch/skills");
    for (name, body) in skills {
        std::fs::create_dir_all(dir.join(name)).expect("skill dir");
        std::fs::write(
            dir.join(name).join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\n{body}\n"),
        )
        .expect("SKILL.md");
    }
    runtime::skill::discover_skills(&[(runtime::skill::SkillScope::Repo, dir)])
}

#[test]
fn mention_token_requires_word_boundary_and_covers_whole_token() {
    for (input, cursor, expected) in [
        ("@", 1, Some(("", 0..1))),
        ("see @src/ma", 11, Some(("src/ma", 4..11))),
        ("see @src/main.rs now", 9, Some(("src/", 4..16))),
        (
            "日本語 @読",
            "日本語 @読".len(),
            Some(("読", "日本語 ".len().."日本語 @読".len())),
        ),
        ("mail a@b.c", 10, None),
        ("@done then", 10, None),
        ("", 0, None),
    ] {
        let actual = mention_at(input, cursor).map(|m| (m.query, m.range));
        assert_eq!(actual, expected, "{input:?} @ {cursor}");
    }
}

#[test]
fn ranking_prefers_name_prefix_then_shallow_paths() {
    let index = index(
        &[
            "crates/gui/src/main.rs",
            "src/main.rs",
            "docs/domain.md",
            "README.md",
        ],
        &[("maintain", "keeps things tidy")],
    );
    assert_eq!(
        labels(&index, "@main"),
        [
            "@maintain",
            "@src/main.rs",
            "@crates/gui/src/main.rs",
            "@docs/domain.md"
        ]
    );
    // An empty query browses the project root, folders and skills included.
    assert_eq!(
        labels(&index, "@"),
        ["@crates/", "@docs/", "@maintain", "@README.md", "@src/"]
    );
    // A path query lists a folder's direct children before deeper descendants.
    assert_eq!(
        labels(&index, "@crates/gui/"),
        ["@crates/gui/src/", "@crates/gui/src/main.rs"]
    );
    assert!(labels(&index, "@zzz").is_empty());
}

#[test]
fn accepting_a_folder_keeps_browsing_while_files_and_skills_end_the_token() {
    let index = index(&["src/main.rs"], &[("review", "reviews")]);
    let input = "look at @sr please";
    let mention = mention_at(input, "look at @sr".len()).expect("mention");
    let items = index.complete(&mention);
    let dir = items
        .iter()
        .find(|item| item.kind == CompletionKind::Dir)
        .expect("dir");
    let file = items
        .iter()
        .find(|item| item.kind == CompletionKind::File)
        .expect("file");
    assert_eq!(
        (dir.replacement.as_str(), dir.range.clone()),
        ("@src/", 8..11)
    );
    assert_eq!(file.replacement, "@src/main.rs ");
    let skill = &index.complete(&mention_at("@rev", 4).expect("mention"))[0];
    assert_eq!(skill.replacement, "@review ");
    // A token that already spells the candidate is left for Enter to send.
    let exact = &index.complete(&mention_at("@review", 7).expect("mention"))[0];
    assert!(!exact.changes("@review"));
    assert!(skill.changes("@rev"));
    assert!(dir.changes(input));
}

#[test]
fn completions_are_capped() {
    let paths: Vec<_> = (0..20).map(|i| format!("file{i:02}.rs")).collect();
    let paths: Vec<_> = paths.iter().map(String::as_str).collect();
    assert_eq!(labels(&index(&paths, &[]), "@file").len(), MAX_COMPLETIONS);
}

#[test]
fn skill_mentions_inline_bodies_that_split_back_to_the_typed_text() {
    let temp = tempfile::tempdir().expect("root");
    let skills = skill_registry(
        temp.path(),
        &[
            ("review", "Check </skill> markers\nline two"),
            ("docs", "docs body"),
        ],
    );
    std::fs::create_dir(temp.path().join("docs")).expect("docs folder");
    let typed = "use @review, then @review again on @docs and @unknown";

    let expanded = expand_skill_mentions(typed, Some(temp.path()), &skills).expect("expand");

    // The skill body is attached once; a project folder named like a skill stays a path.
    assert_eq!(
        expanded,
        format!("{typed}\n\n<skill name=\"review\">\nCheck </skill> markers\nline two\n</skill>")
    );
    assert_eq!(split_skill_attachments(&expanded), (typed, vec!["review"]));
    assert_eq!(split_skill_attachments(typed), (typed, vec![]));
    assert_eq!(
        expand_skill_mentions("plain text", Some(temp.path()), &skills).expect("plain"),
        "plain text"
    );
}

#[test]
fn unreadable_skill_reports_its_name() {
    let temp = tempfile::tempdir().expect("root");
    let skills = skill_registry(temp.path(), &[("gone", "body")]);
    std::fs::remove_file(temp.path().join(".evorch/skills/gone/SKILL.md")).expect("remove");
    assert_eq!(
        expand_skill_mentions("run @gone", Some(temp.path()), &skills),
        Err("gone".into())
    );
}

#[test]
fn git_index_honours_gitignore_and_lists_untracked_files() {
    let temp = tempfile::tempdir().expect("root");
    let root = temp.path();
    let mut init = std::process::Command::new("git");
    for variable in GIT_LOCAL_ENV {
        init.env_remove(variable);
    }
    let status = init
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git init");
    assert!(status.success());
    std::fs::create_dir_all(root.join("src")).expect("src");
    std::fs::create_dir_all(root.join("build")).expect("build");
    std::fs::write(root.join(".gitignore"), "build/\n").expect("ignore");
    std::fs::write(root.join("src/lib.rs"), "").expect("lib");
    std::fs::write(root.join("build/out.bin"), "").expect("out");

    let index = MentionIndex::build(root, &skill_registry(root, &[("lint", "body")]));

    let values: Vec<_> = index
        .entries
        .iter()
        .map(|entry| (entry.kind, entry.value.as_str()))
        .collect();
    assert_eq!(
        values,
        [
            (CompletionKind::Dir, ".evorch"),
            (CompletionKind::Dir, ".evorch/skills"),
            (CompletionKind::Dir, ".evorch/skills/lint"),
            (CompletionKind::Dir, "src"),
            (CompletionKind::File, ".evorch/skills/lint/SKILL.md"),
            (CompletionKind::File, ".gitignore"),
            (CompletionKind::File, "src/lib.rs"),
            (CompletionKind::Skill, "lint"),
        ]
    );
}

#[test]
fn non_git_index_walks_the_tree_skipping_build_output() {
    let temp = tempfile::tempdir().expect("root");
    let root = temp.path();
    for path in ["src/lib.rs", "target/debug/app", "node_modules/x/index.js"] {
        std::fs::create_dir_all(root.join(path).parent().expect("parent")).expect("dir");
        std::fs::write(root.join(path), "").expect("file");
    }
    let index = MentionIndex::build(root, &skill_registry(root, &[]));
    let values: Vec<_> = index
        .entries
        .iter()
        .map(|entry| entry.value.as_str())
        .collect();
    assert_eq!(values, ["src", "src/lib.rs"]);
}
