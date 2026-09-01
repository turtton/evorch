//! ツール出力に含まれる制御マーカのエスケープ。

/// v0.1 でエスケープ対象とする開始マーカ。
const OPEN_MARKER: &str = "<system-reminder>";

/// v0.1 でエスケープ対象とする終了マーカ。
const CLOSE_MARKER: &str = "</system-reminder>";

/// 入力中のシステムリマインダ制御マーカをエスケープする。
///
/// v0.1 の対象は `<system-reminder>` と `</system-reminder>` の 2 種類で、
/// `<` の直後に `\` を挿入する（例: `<\system-reminder>`、`<\/system-reminder>`）。
/// マーカ以外のテキストはバイト単位で変更しない。挿入した `\` により置換結果が
/// 再びマーカに一致することはないため、この関数は冪等である。
pub fn escape_control_markers(input: &str) -> String {
    let open_escaped = format!("<\\{}", &OPEN_MARKER[1..]);
    let close_escaped = format!("<\\{}", &CLOSE_MARKER[1..]);
    input
        .replace(OPEN_MARKER, &open_escaped)
        .replace(CLOSE_MARKER, &close_escaped)
}

/// JSON 値内の全文字列値に含まれる制御マーカを再帰的にエスケープする。
///
/// detail のようなツール提供メタデータはサーバー制御の文字列 (request_id 等) を
/// 含み得るため、本文と同様に扱う。オブジェクトの key はプロトコル上の識別子
/// でありエスケープしない。数値・真偽値・null はそのまま保持する。
pub fn escape_control_markers_in_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(escape_control_markers(&text)),
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(escape_control_markers_in_value)
                .collect(),
        ),
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, escape_control_markers_in_value(value)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 開始マーカを含む入力 / When: escape_control_markers / Then: '<' の直後に '\' が挿入される
    #[test]
    fn sanitize_escapes_open_marker() {
        let escaped = escape_control_markers("before <system-reminder> after");

        assert_eq!(escaped, "before <\\system-reminder> after");
    }

    // Given: 終了マーカを含む入力 / When: escape_control_markers / Then: '<' の直後に '\' が挿入される
    #[test]
    fn sanitize_escapes_close_marker() {
        let escaped = escape_control_markers("before </system-reminder> after");

        assert_eq!(escaped, "before <\\/system-reminder> after");
    }

    // Given: マーカーを含むネストした detail 値 / When: escape_control_markers_in_value / Then: 文字列値のみエスケープされ key と非文字列は不変
    #[test]
    fn sanitize_value_escapes_markers_in_nested_strings() {
        let input = serde_json::json!({
            "a": "<system-reminder>",
            "nested": { "b": ["</system-reminder>", 1, null, true] },
            "n": 3,
        });

        let escaped = escape_control_markers_in_value(input);

        assert_eq!(
            escaped,
            serde_json::json!({
                "a": "<\\system-reminder>",
                "nested": { "b": ["<\\/system-reminder>", 1, null, true] },
                "n": 3,
            })
        );
    }

    // Given: マーカーを含まない detail 値 / When: escape_control_markers_in_value / Then: 値がそのまま返る
    #[test]
    fn sanitize_value_leaves_marker_free_value_untouched() {
        let input = serde_json::json!({ "request_id": "req-1", "count": 2, "ok": true });

        assert_eq!(escape_control_markers_in_value(input.clone()), input);
    }

    // Given: マーカを含む入力 / When: 2 回連続でエスケープ / Then: 1 回の結果と等しくなる（冪等）
    #[test]
    fn sanitize_is_idempotent() {
        let input = "a <system-reminder> b </system-reminder> c";
        let once = escape_control_markers(input);
        let twice = escape_control_markers(&once);

        assert_eq!(twice, once);
        assert_eq!(
            escape_control_markers("<\\system-reminder> <\\/system-reminder>"),
            "<\\system-reminder> <\\/system-reminder>"
        );
    }

    // Given: マーカを含まない入力 / When: escape_control_markers / Then: 入力がバイト単位でそのまま返る
    #[test]
    fn sanitize_leaves_plain_text_untouched() {
        assert_eq!(
            escape_control_markers("README.md を読んで <div> 123"),
            "README.md を読んで <div> 123"
        );
        assert_eq!(escape_control_markers(""), "");
    }

    // Given: マーカが入力の先頭・末尾・隣接する入力 / When: escape_control_markers / Then: 位置によらずすべてエスケープされる
    #[test]
    fn sanitize_handles_marker_at_boundaries() {
        assert_eq!(
            escape_control_markers("<system-reminder>head</system-reminder>"),
            "<\\system-reminder>head<\\/system-reminder>"
        );
        assert_eq!(
            escape_control_markers("<system-reminder></system-reminder>"),
            "<\\system-reminder><\\/system-reminder>"
        );
        assert_eq!(
            escape_control_markers("tail </system-reminder>"),
            "tail <\\/system-reminder>"
        );
    }
}
