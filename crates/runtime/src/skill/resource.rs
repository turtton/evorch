//! SKILL.md 本文から参照されるバンドルリソースの解決・読み出し (issue #53 / AC2)。
//!
//! 不変条件:
//! - エラー Display はリファレンス文字列と理由クラスのみを運び、ファイル内容や
//!   OS エラー文字列 (フルパスを含む) を漏らさない。
//! - リファレンスは skill ルート相対かつ 1 階層まで (`file` または `dir/file`)
//!   のみ受理する。`..` / `.` / 絶対パス / バックスラッシュは検証で拒否し、
//!   さらに canonicalize による symlink 脱出防止を defense-in-depth で行う。

use std::fs;
use std::path::Path;

/// リソース読み出しエラー。Display はリファレンス文字列と理由のみを運び、
/// ファイル内容や OS エラー文字列 (フルパスを含む) を漏らさない。
#[derive(Debug, thiserror::Error)]
pub enum SkillResourceError {
    /// リファレンスが形状規約に違反している。ペイロードは理由クラス
    /// (frontmatter.rs の `InvalidYaml` と同一の運用)。
    #[error("リファレンスが不正です: {0}")]
    InvalidReference(String),
    /// リファレンスに対応するファイルが存在しない (または読めない)。
    #[error("リファレンス '{0}' に対応するファイルがありません")]
    NotFound(String),
    /// ファイル内容が UTF-8 として解釈できない。
    #[error("リファレンス '{0}' の内容は UTF-8 である必要があります")]
    NotUtf8(String),
    /// symlink 解決後の実体が skill ディレクトリの外にある。
    #[error("リファレンス '{0}' は skill ディレクトリの外を指します")]
    OutsideSkillDir(String),
}

/// SKILL.md 本文が参照するバンドルリソースを読み出す。
///
/// `reference` は skill ルート相対かつ 1 階層まで (`file` または `dir/file`)。
/// まず純粋な形状検証でリファレンスを固定し、その後
/// `skill_dir.join(reference)` を canonicalize して、実体が skill_dir の
/// canonical パス配下に収まることを確認する (symlink による脱出の防御)。
/// fs 由来のエラーは理由をリークしないようすべてリファレンス添付の型付き
/// エラーへ折り畳む。
pub fn read_skill_resource(
    skill_dir: &Path,
    reference: &str,
) -> Result<String, SkillResourceError> {
    validate_reference(reference)
        .map_err(|reason| SkillResourceError::InvalidReference(reason.to_owned()))?;
    let resolved = skill_dir.join(reference);
    let canonical_file = resolved
        .canonicalize()
        .map_err(|_| SkillResourceError::NotFound(reference.to_owned()))?;
    let canonical_dir = skill_dir
        .canonicalize()
        .map_err(|_| SkillResourceError::NotFound(reference.to_owned()))?;
    if !canonical_file.starts_with(&canonical_dir) {
        return Err(SkillResourceError::OutsideSkillDir(reference.to_owned()));
    }
    let bytes = fs::read(&canonical_file)
        .map_err(|_| SkillResourceError::NotFound(reference.to_owned()))?;
    String::from_utf8(bytes).map_err(|_| SkillResourceError::NotUtf8(reference.to_owned()))
}

/// リファレンス形状を検証する: 空でないこと、バックスラッシュを含まないこと、
/// 絶対パスでないこと、セグメントが `.` / `..` / 空でないこと、2 セグメント
/// 以下であること。違反時は理由クラスを返す。
///
/// `Path::components()` は unix でカレントコンポーネント `.` を暗黙に正規化
/// して消すため、`scripts/./x.sh` のような参照を捕えられない。そこで
/// `/` での手動分割によって `.` を明示的に検出する。
fn validate_reference(reference: &str) -> Result<(), &'static str> {
    if reference.is_empty() {
        return Err("empty reference");
    }
    if reference.contains('\\') {
        return Err("backslash separator");
    }
    if reference.starts_with('/') || Path::new(reference).is_absolute() {
        return Err("absolute path");
    }
    let segments: Vec<&str> = reference.split('/').collect();
    for segment in &segments {
        match *segment {
            "." => return Err("current component '.'"),
            ".." => return Err("parent component '..'"),
            "" => return Err("empty path segment"),
            _ => {}
        }
    }
    if segments.len() > 2 {
        return Err("path exceeds one directory level");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    /// Given/When 用ヘルパー: dir 下の relative にファイルを作る (親ディレクトリも作成)。
    fn write_skill_file(dir: &Path, relative: &str, contents: &[u8]) {
        let path = dir.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Given: skill_dir/references/x.md が存在する
    /// When:  read_skill_resource(dir, "references/x.md") を呼ぶ
    /// Then:  ファイル内容そのものが返る
    #[test]
    fn reads_one_level_reference() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "references/x.md", b"# hello\n");
        let content = read_skill_resource(dir.path(), "references/x.md").unwrap();
        assert_eq!(content, "# hello\n");
    }

    /// Given: skill_dir 直下に NOTES.md が存在する
    /// When:  read_skill_resource(dir, "NOTES.md") を呼ぶ
    /// Then:  ファイル内容そのものが返る
    #[test]
    fn reads_root_level_file() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "NOTES.md", b"root note\n");
        let content = read_skill_resource(dir.path(), "NOTES.md").unwrap();
        assert_eq!(content, "root note\n");
    }

    /// Given: 空の skill ディレクトリ
    /// When:  read_skill_resource(dir, "references/a/b.md") を呼ぶ
    /// Then:  2 階層超えは InvalidReference (深度規則)
    #[test]
    fn rejects_two_level_reference_by_depth() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_skill_resource(dir.path(), "references/a/b.md").unwrap_err();
        assert!(matches!(err, SkillResourceError::InvalidReference(_)));
    }

    /// Given: 空の skill ディレクトリ
    /// When:  read_skill_resource(dir, "../evil.md") を呼ぶ
    /// Then:  親コンポーネントは InvalidReference (fs に触れる前に拒否)
    #[test]
    fn rejects_parent_component() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_skill_resource(dir.path(), "../evil.md").unwrap_err();
        assert!(matches!(err, SkillResourceError::InvalidReference(_)));
    }

    /// Given: 空の skill ディレクトリ
    /// When:  read_skill_resource(dir, "/etc/passwd") を呼ぶ
    /// Then:  絶対パスは InvalidReference
    #[test]
    fn rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_skill_resource(dir.path(), "/etc/passwd").unwrap_err();
        assert!(matches!(err, SkillResourceError::InvalidReference(_)));
    }

    /// Given: 空の skill ディレクトリ
    /// When:  read_skill_resource(dir, "scripts/./x.sh") を呼ぶ
    /// Then:  カレントコンポーネント `.` は InvalidReference
    #[test]
    fn rejects_current_component() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_skill_resource(dir.path(), "scripts/./x.sh").unwrap_err();
        assert!(matches!(err, SkillResourceError::InvalidReference(_)));
    }

    /// Given: 空の skill ディレクトリ
    /// When:  read_skill_resource(dir, "refs\\x.md") を呼ぶ
    /// Then:  バックスラッシュ区切りは InvalidReference (unix-first でも fail closed)
    #[test]
    fn rejects_backslash_separator() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_skill_resource(dir.path(), "refs\\x.md").unwrap_err();
        assert!(matches!(err, SkillResourceError::InvalidReference(_)));
    }

    /// Given: 空の skill ディレクトリ
    /// When:  read_skill_resource(dir, "") を呼ぶ
    /// Then:  空リファレンスは InvalidReference
    #[test]
    fn rejects_empty_reference() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_skill_resource(dir.path(), "").unwrap_err();
        assert!(matches!(err, SkillResourceError::InvalidReference(_)));
    }

    /// Given: references/ ディレクトリは存在するが nope.md はない
    /// When:  read_skill_resource(dir, "references/nope.md") を呼ぶ
    /// Then:  NotFound (OS エラー文字列は運ばない)
    #[test]
    fn not_found_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "references/x.md", b"# hello\n");
        let err = read_skill_resource(dir.path(), "references/nope.md").unwrap_err();
        assert!(matches!(err, SkillResourceError::NotFound(_)));
        assert!(!err.to_string().contains(dir.path().to_str().unwrap()));
    }

    /// Given: 非 UTF-8 バイナリファイルが存在する
    /// When:  read_skill_resource(dir, "references/bin.md") を呼ぶ
    /// Then:  NotUtf8 であり、内容はエラー文言に現れない
    #[test]
    fn not_utf8_when_binary_content() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "references/bin.md", b"LEAKMARKER\xff\xfe\x00");
        let err = read_skill_resource(dir.path(), "references/bin.md").unwrap_err();
        assert!(matches!(err, SkillResourceError::NotUtf8(_)));
        assert!(!err.to_string().contains("LEAKMARKER"));
    }

    /// Given: skill_dir 外のファイルと、それを指す skill_dir 内のシンボリックリンク
    /// When:  リンク先リファレンスで read_skill_resource を呼ぶ
    /// Then:  canonicalize による脱出防止で OutsideSkillDir
    #[test]
    fn rejects_symlink_escaping_skill_dir() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "references/x.md", b"# hello\n");
        symlink(
            outside.path().join("secret.txt"),
            dir.path().join("references/leak.md"),
        )
        .unwrap();
        let err = read_skill_resource(dir.path(), "references/leak.md").unwrap_err();
        assert!(matches!(err, SkillResourceError::OutsideSkillDir(_)));
    }

    /// Given: references/a/b.md が実ディスクに存在する
    /// When:  read_skill_resource(dir, "references/a/b.md") を呼ぶ
    /// Then:  ディスク状態によらず深度規則で InvalidReference (fs アクセス前の拒否)
    #[test]
    fn depth_rule_fires_before_fs_access() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_file(dir.path(), "references/a/b.md", b"# deep\n");
        let err = read_skill_resource(dir.path(), "references/a/b.md").unwrap_err();
        assert!(matches!(err, SkillResourceError::InvalidReference(_)));
    }
}
