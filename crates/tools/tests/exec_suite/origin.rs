//! [`derive_content_origin`] の写像表テスト。

use tools::{ContentOrigin, Permissions, derive_content_origin};

// Given: 各権限の組合せ / When: derive_content_origin で導出 / Then: 契約表どおりの由来になる
//
// network が最優先で WebUntrusted、非ネットワークの読み取り専用は
// RepositoryUntrusted、書き込み・プロセス起動を伴うローカル権限は ToolTrusted。
#[test]
fn derive_content_origin_mapping_table() {
    assert_eq!(
        derive_content_origin(&Permissions::network()),
        ContentOrigin::WebUntrusted
    );
    assert_eq!(
        derive_content_origin(&Permissions::read_only()),
        ContentOrigin::RepositoryUntrusted
    );
    assert_eq!(
        derive_content_origin(&Permissions::read_write()),
        ContentOrigin::ToolTrusted
    );
    assert_eq!(
        derive_content_origin(&Permissions::process()),
        ContentOrigin::ToolTrusted
    );
    assert_eq!(
        derive_content_origin(&Permissions {
            fs_read: true,
            fs_write: true,
            process_spawn: false,
            network: true,
        }),
        ContentOrigin::WebUntrusted
    );
}
