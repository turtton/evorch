//! ツール出力の由来を表す型と権限からの機械導出を定義します。

use crate::tool::Permissions;
use serde::{Deserialize, Serialize};

/// ツール出力本文の由来。
///
/// ツール自身の申告ではなく [`Tool`](crate::tool::Tool) の権限宣言から
/// 機械導出する (AC5)。untrusted 由来の本文は下流 (モデル注入・GUI 表示)
/// での扱いを区別するための注釈となる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentOrigin {
    /// ネットワーク由来 (web_search / web_fetch 等の外部コンテンツ)。
    WebUntrusted,
    /// リポジトリ由来 (read / grep が返したファイル内容)。
    RepositoryUntrusted,
    /// 信頼できるツール自身の生成出力。
    ToolTrusted,
}

/// ツールの権限宣言から出力の由来を導出する。
///
/// ネットワークアクセスを持つなら [`ContentOrigin::WebUntrusted`]、そうでなく
/// 読み取り専用 (`fs_read` のみ `true`) なら
/// [`ContentOrigin::RepositoryUntrusted`]、それ以外は
/// [`ContentOrigin::ToolTrusted`]。
pub const fn derive_content_origin(permissions: &Permissions) -> ContentOrigin {
    if permissions.network {
        ContentOrigin::WebUntrusted
    } else if permissions.fs_read && !permissions.fs_write && !permissions.process_spawn {
        ContentOrigin::RepositoryUntrusted
    } else {
        ContentOrigin::ToolTrusted
    }
}
