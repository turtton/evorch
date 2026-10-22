//! edit ツールの実装。
//!
//! 空でない `old_string` の一致箇所を `new_string` へ置換する。`replace_all` が
//! なければ一致は 1 箇所に限る。`edits` の複数置換はすべて元の内容に対して照合し、
//! 見つからない・一意でない・重なる置換が 1 つでもあれば何も書き込まない。
//! 書き込みは必ず同一親ディレクトリ上の一時ファイル経由で行い、`persist` に
//! よる原子的リネームで反映する。ディスクへの書き込みはバイト一致とし、制御
//! マーカーのエスケープは行わない（ADR 0008。エスケープは wave 3 の
//! ToolExecutor が結果正規化で担う）。例外として改行は CRLF のファイルに
//! そろえる。すべての改行が CRLF のファイルでは `old_string` と `new_string` の
//! 単独の LF を CRLF として扱う。CRLF と LF が混在するファイルでは、LF 改行の
//! `old_string` が CRLF に読み替えたときだけ一致する場合に限り、両方を CRLF にする。

use std::borrow::Cow;
use std::io::Write;
use std::sync::Arc;

use serde_json::Value;

use crate::post_edit::{
    PostEditHook, PostEditInput, PostEditReplacement, UnavailableReason, apply_post_edit,
    apply_unavailable, input_fits,
};

use std::path::{Path, PathBuf};

use super::file_diff;
use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};

/// 一意でない一致の報告で行番号を示す一致の上限。
const MAX_REPORTED_MATCHES: usize = 10;

/// ファイル内の文字列を置換するツール。
#[derive(Clone, Default)]
pub struct Edit {
    post_edit: Option<Arc<dyn PostEditHook>>,
}

// Preserve the existing hook-free value constructor as well as the type name.
#[allow(non_upper_case_globals)]
pub const Edit: Edit = Edit { post_edit: None };

impl std::fmt::Debug for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Edit")
            .field("post_edit", &self.post_edit.is_some())
            .finish()
    }
}

impl Edit {
    pub fn with_post_edit_hook(mut self, hook: Arc<dyn PostEditHook>) -> Self {
        self.post_edit = Some(hook);
        self
    }
}

#[async_trait::async_trait]
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace exact text in an existing file. Prefer this over shell sed -i, heredocs or scripts for changing files. Read the file first. Unless replace_all is true, old_string must match exactly one location; otherwise the call fails and the file is unchanged. Send several changes to one file together in edits: each is matched against the original contents, matches must not overlap, and either all are applied or none. Returns the diff. To create or replace a whole file, use write(path, content)."
    }

    fn schema(&self) -> serde_json::Value {
        super::file_access::with_access_arguments(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Exact text to replace, including whitespace and line breaks."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text; empty deletes old_string."
                },
                "replace_all": {
                    "type": "boolean",
                    "default": false,
                    "description": "Replace every match of old_string instead of requiring exactly one."
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Several replacements in this file, instead of old_string/new_string/replace_all. Each item follows the same rules.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string", "minLength": 1 },
                            "new_string": { "type": "string" },
                            "replace_all": { "type": "boolean", "default": false }
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }))
    }

    /// 編集の承認分類は不変。追加検査のプロセス起動は executor が別途承認する。
    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    /// 既存ファイルへ置換をすべて適用するか、どれも適用しない。
    ///
    /// # Errors
    /// 引数が不正なとき、または `edits` の一致範囲が重なるときは
    /// [`ToolError::InvalidArgs`]、ファイルが存在しなければ
    /// [`ToolError::PathNotFound`]、置換対象が見つからなければ
    /// [`ToolError::EditTargetNotFound`]、`replace_all` なしで複数箇所に一致すれば
    /// [`ToolError::EditTargetNotUnique`]、入出力に失敗すれば [`ToolError::Io`] を返す。
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let path_text = required_str(&args, "path")?;
        let replacements = replacements(&args)?;
        let path = Path::new(path_text);
        let current = std::fs::read_to_string(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolError::PathNotFound {
                    path: path_text.to_string(),
                }
            } else {
                io_error("ファイルの読み込みに失敗しました", error)
            }
        })?;
        let endings = LineEndings::of(&current);
        let targets: Vec<_> = replacements
            .iter()
            .map(|replacement| Target::new(&current, replacement, endings))
            .collect();
        let mut spans = Vec::new();
        for (owner, (replacement, target)) in replacements.iter().zip(&targets).enumerate() {
            for start in match_starts(&current, path_text, replacement, &target.old)? {
                spans.push(Span {
                    start,
                    end: start + target.old.len(),
                    owner,
                });
            }
        }
        spans.sort_by_key(|span| span.start);
        if let Some(pair) = spans.windows(2).find(|pair| pair[0].end > pair[1].start) {
            let mut owners = [pair[0].owner, pair[1].owner];
            owners.sort_unstable();
            return Err(invalid(format!(
                "{} and {} overlap in {path_text} (line {}); merge them into one edit or match separate text",
                replacements[owners[0]].name(),
                replacements[owners[1]].name(),
                line_numbers(&current, &[pair[1].start])[0],
            )));
        }
        let mut next_content = String::with_capacity(current.len());
        let mut copied = 0;
        for span in &spans {
            next_content.push_str(&current[copied..span.start]);
            next_content.push_str(&targets[span.owner].new);
            copied = span.end;
        }
        next_content.push_str(&current[copied..]);
        let mut result = ToolResult::success(file_diff::changed_file(
            path_text,
            Some(&current),
            &next_content,
        ));
        if next_content == current {
            return Ok(result);
        }
        write_atomically(path, &next_content)?;
        if let Some(hook) = &self.post_edit {
            check_changed_pairs(hook.as_ref(), path, &targets, &mut result).await;
        }
        Ok(result)
    }
}

/// 引数で指定された 1 つの置換。
struct Replacement<'a> {
    /// `edits` 形式での添字。単一置換では `None`。
    edit: Option<usize>,
    old: &'a str,
    new: &'a str,
    replace_all: bool,
}

impl Replacement<'_> {
    fn name(&self) -> String {
        self.edit.map_or_else(
            || "old_string".to_owned(),
            |index| format!("edits[{index}]"),
        )
    }
}

/// 編集前のファイルの改行。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineEndings {
    /// CRLF を含まない。
    Lf,
    /// すべての改行が CRLF。
    Crlf,
    /// CRLF と単独の LF が混在する。
    Mixed,
}

impl LineEndings {
    fn of(content: &str) -> Self {
        match (content.contains("\r\n"), has_bare_lf(content)) {
            (false, _) => Self::Lf,
            (true, false) => Self::Crlf,
            (true, true) => Self::Mixed,
        }
    }
}

/// 照合と書き込みに使う文字列。CRLF に読み替えた場合は変換後の値を持つ。
struct Target<'a> {
    old: Cow<'a, str>,
    new: Cow<'a, str>,
}

impl<'a> Target<'a> {
    fn new(content: &str, replacement: &Replacement<'a>, endings: LineEndings) -> Self {
        let crlf = match endings {
            LineEndings::Lf => false,
            LineEndings::Crlf => true,
            // A mixed file has no style to follow; convert only what the match needs.
            LineEndings::Mixed => {
                has_bare_lf(replacement.old) && !content.contains(replacement.old)
            }
        };
        if crlf {
            Self {
                old: to_crlf(replacement.old),
                new: to_crlf(replacement.new),
            }
        } else {
            Self {
                old: Cow::Borrowed(replacement.old),
                new: Cow::Borrowed(replacement.new),
            }
        }
    }
}

/// 元の内容のうち置き換えるバイト範囲。
struct Span {
    start: usize,
    end: usize,
    /// 範囲を指定した置換の、引数での位置。
    owner: usize,
}

/// 単一置換または `edits` のどちらか一方を取り出す。
fn replacements(args: &Value) -> Result<Vec<Replacement<'_>>, ToolError> {
    let Some(edits) = args.get("edits") else {
        if args.get("old_string").is_none() && args.get("new_string").is_none() {
            return Err(invalid(
                "provide old_string and new_string, or edits for several replacements",
            ));
        }
        let old = required_str(args, "old_string")?;
        if old.is_empty() {
            return Err(invalid(
                "old_string must be non-empty; use write(path, content) to create or replace a file",
            ));
        }
        return Ok(vec![Replacement {
            edit: None,
            old,
            new: required_str(args, "new_string")?,
            replace_all: replace_all(args, "")?,
        }]);
    };
    if ["old_string", "new_string", "replace_all"]
        .iter()
        .any(|key| args.get(key).is_some())
    {
        return Err(invalid(
            "use either old_string/new_string/replace_all or edits, not both; put every replacement in edits",
        ));
    }
    let edits = edits
        .as_array()
        .filter(|edits| !edits.is_empty())
        .ok_or_else(|| {
            invalid("edits must be a non-empty array of {old_string, new_string, replace_all?}")
        })?;
    edits
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            let label = format!("edits[{index}].");
            let text = |key: &str| {
                edit.get(key)
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid(format!("{label}{key} must be a string")))
            };
            let old = text("old_string")?;
            if old.is_empty() {
                return Err(invalid(format!("{label}old_string must be non-empty")));
            }
            Ok(Replacement {
                edit: Some(index),
                old,
                new: text("new_string")?,
                replace_all: replace_all(edit, &label)?,
            })
        })
        .collect()
}

fn replace_all(value: &Value, label: &str) -> Result<bool, ToolError> {
    value.get("replace_all").map_or(Ok(false), |flag| {
        flag.as_bool()
            .ok_or_else(|| invalid(format!("{label}replace_all must be a boolean")))
    })
}

/// 置換する一致の開始位置を昇順で返す。
fn match_starts(
    content: &str,
    path: &str,
    replacement: &Replacement<'_>,
    needle: &str,
) -> Result<Vec<usize>, ToolError> {
    let not_found = || ToolError::EditTargetNotFound {
        path: path.to_owned(),
        edit: replacement.edit,
    };
    let all = || {
        content
            .match_indices(needle)
            .map(|(start, _)| start)
            .collect::<Vec<_>>()
    };
    if replacement.replace_all {
        let starts = all();
        return if starts.is_empty() {
            Err(not_found())
        } else {
            Ok(starts)
        };
    }
    let first = content.find(needle).ok_or_else(not_found)?;
    // A second match may overlap the first, which match_indices would skip.
    let after_first = first + content[first..].chars().next().map_or(1, char::len_utf8);
    let Some(offset) = content[after_first..].find(needle) else {
        return Ok(vec![first]);
    };
    let mut starts = all();
    if starts.len() == 1 {
        starts.push(after_first + offset);
    }
    Err(ToolError::EditTargetNotUnique {
        path: path.to_owned(),
        edit: replacement.edit,
        count: starts.len(),
        lines: line_numbers(content, &starts[..starts.len().min(MAX_REPORTED_MATCHES)]),
    })
}

/// 昇順の開始位置それぞれの 1 始まりの行番号。
fn line_numbers(content: &str, starts: &[usize]) -> Vec<usize> {
    let mut line = 1;
    let mut counted = 0;
    starts
        .iter()
        .map(|&start| {
            line += content.as_bytes()[counted..start]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count();
            counted = start;
            line
        })
        .collect()
}

fn has_bare_lf(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .iter()
        .enumerate()
        .any(|(index, &byte)| byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r'))
}

fn to_crlf(text: &str) -> Cow<'_, str> {
    if !has_bare_lf(text) {
        return Cow::Borrowed(text);
    }
    let mut converted = String::with_capacity(text.len() + text.len() / 8);
    let mut previous = None;
    for character in text.chars() {
        if character == '\n' && previous != Some('\r') {
            converted.push('\r');
        }
        converted.push(character);
        previous = Some(character);
    }
    Cow::Owned(converted)
}

/// 内容を変えた組だけを検査する。1 組なら upstream の Edit、複数なら MultiEdit。
async fn check_changed_pairs(
    hook: &dyn PostEditHook,
    path: &Path,
    targets: &[Target<'_>],
    result: &mut ToolResult,
) {
    let changed: Vec<_> = targets
        .iter()
        .filter(|target| target.old != target.new)
        .collect();
    let parts: Vec<&str> = changed
        .iter()
        .flat_map(|target| [target.old.as_ref(), target.new.as_ref()])
        .collect();
    if !input_fits(path, &parts) {
        apply_unavailable(
            hook,
            UnavailableReason::InputTooLarge,
            "checker input exceeds 2 MiB",
            result,
        );
        return;
    }
    let input = match changed.as_slice() {
        [target] => PostEditInput {
            tool_name: "Edit",
            file_path: path.to_path_buf(),
            content: None,
            old_string: Some(target.old.clone().into_owned()),
            new_string: Some(target.new.clone().into_owned()),
            edits: None,
        },
        _ => PostEditInput {
            tool_name: "MultiEdit",
            file_path: path.to_path_buf(),
            content: None,
            old_string: None,
            new_string: None,
            edits: Some(
                changed
                    .iter()
                    .map(|target| PostEditReplacement {
                        old_string: target.old.clone().into_owned(),
                        new_string: target.new.clone().into_owned(),
                    })
                    .collect(),
            ),
        },
    };
    apply_post_edit(hook, &input, result).await;
}

fn invalid(detail: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        detail: detail.into(),
    }
}

/// `args` から必須の文字列プロパティを取り出す。
pub(super) fn required_str<'a>(
    args: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::InvalidArgs {
            detail: format!("文字列のプロパティ {key} が必要です"),
        })
}

/// `content` を `path` へ原子的に書き込む。
///
/// 同一親ディレクトリに一時ファイルを作成して書き出した後、`persist` で `path`
/// へリネームする。失敗時は一時ファイルが drop 時に削除される。
pub(super) fn write_atomically(path: &Path, content: &str) -> Result<(), ToolError> {
    let parent_dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut temp = tempfile::NamedTempFile::new_in(&parent_dir)
        .map_err(|error| io_error("一時ファイルの作成に失敗しました", error))?;
    // 原子的な置換で既存ファイルの実行ビット等を失わない。
    match std::fs::metadata(path) {
        Ok(metadata) => temp
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(|error| io_error("ファイル権限の維持に失敗しました", error))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("ファイル情報の読み込みに失敗しました", error)),
    }
    temp.write_all(content.as_bytes())
        .map_err(|error| io_error("一時ファイルへの書き込みに失敗しました", error))?;
    temp.flush()
        .map_err(|error| io_error("一時ファイルのフラッシュに失敗しました", error))?;
    temp.persist(path)
        .map_err(|error| io_error("一時ファイルの確定に失敗しました", error.error))?;
    Ok(())
}

/// 入出力の失敗を段階名付きの [`ToolError::Io`] へ変換する。
fn io_error(stage: &str, error: std::io::Error) -> ToolError {
    ToolError::Io {
        detail: format!("{stage}: {error}"),
    }
}
