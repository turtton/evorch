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
