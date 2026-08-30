//! 現行設定スキーマを標準出力、または指定パスのファイルへ出力する例です。
//!
//! versioned artifact (`docs/config/evorch-config-v{n}.schema.json`) の再生成は
//! ファイル出力で行う: `cargo run -p config --example dump_schema -- <path>`

use std::path::PathBuf;

fn main() {
    let schema = format!("{}\n", config::json_schema());

    match std::env::args_os().nth(1).map(PathBuf::from) {
        Some(path) => std::fs::write(&path, &schema)
            .unwrap_or_else(|e| panic!("schema を書き出せない: {}: {e}", path.display())),
        None => print!("{schema}"),
    }
}
