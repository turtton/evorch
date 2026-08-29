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
