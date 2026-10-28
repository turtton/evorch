//! read ツールのパス引数の別名を 1 つの `path` に正規化する。
//!
//! モデルは他ツールで慣れた `file` / `file_path` / `filename` / `target` を送ることがある。
//! 別名は schema に載せず（載せると全プロパティを埋めるモデルが複数に別の値を入れる）、
//! 実行前にここで `path` へ畳む。境界判定・審査・実読み取りは畳んだ後の 1 パスだけを見る。

use serde_json::Value;

use crate::error::ToolError;

/// `path` の別名。優先順位はなく、非空の値はすべて一致している必要がある。
const READ_PATH_ALIASES: [&str; 4] = ["file", "file_path", "filename", "target"];

/// 値の表示に使う最大文字数。モデルに返すエラーを肥大させない。
const SHOWN_VALUE_CHARS: usize = 200;

/// `tool_name` のパス別名を `path` に畳む。別名を持たないツールでは何もしない。
///
/// 空文字の別名は無視し、同じ値の重複は 1 つにまとめる。非空で異なる値が複数あれば
/// どのキーにどの値が入っていたかを示して拒否する。拒否時は `args` を変更しない。
///
/// # Errors
/// 別名の値が文字列でない、または非空の値が互いに異なる場合の [`ToolError::InvalidArgs`]。
pub(crate) fn canonicalize_path_aliases(
    tool_name: &str,
    args: &mut Value,
) -> Result<(), ToolError> {
    if tool_name != "read" {
        return Ok(());
    }
    let Some(object) = args.as_object() else {
        return Ok(());
    };
    let mut chosen: Option<&str> = None;
    let mut given: Vec<(&str, &str)> = Vec::new();
    for key in std::iter::once("path").chain(READ_PATH_ALIASES) {
        let Some(value) = object.get(key) else {
            continue;
        };
        let Some(text) = value.as_str() else {
            return Err(ToolError::InvalidArgs {
                detail: format!("引数 {key} は文字列である必要があります"),
            });
        };
        if text.is_empty() {
            continue;
        }
        given.push((key, text));
        match chosen {
            None => chosen = Some(text),
            Some(first) if first != text => {
                return Err(conflict(&given));
            }
            Some(_) => {}
        }
    }
    let chosen = chosen.map(str::to_owned);
    let Some(object) = args.as_object_mut() else {
        return Ok(());
    };
    for key in READ_PATH_ALIASES {
        object.remove(key);
    }
    if let Some(path) = chosen {
        object.insert("path".into(), Value::String(path));
    }
    Ok(())
}

fn conflict(given: &[(&str, &str)]) -> ToolError {
    let listed = given
        .iter()
        .map(|(key, value)| {
            let shown: String = value.chars().take(SHOWN_VALUE_CHARS).collect();
            let ellipsis = if value.chars().count() > SHOWN_VALUE_CHARS {
                "…"
            } else {
                ""
            };
            format!("{key}=\"{shown}{ellipsis}\"")
        })
        .collect::<Vec<_>>()
        .join(", ");
    ToolError::InvalidArgs {
        detail: format!(
            "パスが複数の引数に異なる値で指定されています ({listed})。何も実行していません。パスは path 引数 1 つだけで指定してください"
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn canonical(mut args: Value) -> Result<Value, ToolError> {
        canonicalize_path_aliases("read", &mut args).map(|()| args)
    }

    #[test]
    fn alias_alone_becomes_path() {
        for key in READ_PATH_ALIASES {
            assert_eq!(
                canonical(json!({key: "/a", "offset": 3})).unwrap(),
                json!({"path": "/a", "offset": 3})
            );
        }
    }

    #[test]
    fn equal_duplicates_and_empty_aliases_collapse_to_one_path() {
        assert_eq!(
            canonical(json!({"path": "/a", "file": "/a", "target": "", "filename": ""})).unwrap(),
            json!({"path": "/a"})
        );
        assert_eq!(
            canonical(json!({"path": "", "file_path": "/a"})).unwrap(),
            json!({"path": "/a"})
        );
        assert_eq!(
            canonical(json!({"path": "", "file": ""})).unwrap(),
            json!({"path": ""})
        );
    }

    #[test]
    fn differing_values_are_rejected_without_changing_args() {
        let mut args = json!({"path": "/a/note.md", "file": "/vault", "target": "/vault"});
        let before = args.clone();
        let error = canonicalize_path_aliases("read", &mut args).unwrap_err();
        assert_eq!(args, before);
        let ToolError::InvalidArgs { detail } = error else {
            panic!("expected InvalidArgs");
        };
        assert!(detail.contains("path=\"/a/note.md\""), "{detail}");
        assert!(detail.contains("file=\"/vault\""), "{detail}");
        assert!(detail.contains("path 引数 1 つ"), "{detail}");
    }

    #[test]
    fn non_string_alias_is_rejected_and_other_tools_are_untouched() {
        assert!(canonical(json!({"path": "/a", "file": 7})).is_err());
        let mut args = json!({"path": "/a", "file": "/b"});
        canonicalize_path_aliases("grep", &mut args).unwrap();
        assert_eq!(args, json!({"path": "/a", "file": "/b"}));
    }

    #[test]
    fn long_values_are_cut_in_the_message() {
        let long = "x".repeat(1000);
        let ToolError::InvalidArgs { detail } =
            canonical(json!({"path": long, "file": "/b"})).unwrap_err()
        else {
            panic!("expected InvalidArgs");
        };
        assert!(detail.len() < 600, "{}", detail.len());
    }
}
