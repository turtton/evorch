//! 現行設定スキーマを標準出力へ出力する例です。

fn main() {
    println!("{}", config::json_schema());
}
