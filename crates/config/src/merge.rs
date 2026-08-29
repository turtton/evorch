/// ベース設定へオーバーレイ設定を再帰的にマージします。
#[allow(dead_code)]
pub(crate) fn deep_merge(base: toml::Value, overlay: toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(mut base), toml::Value::Table(overlay)) => {
            for (key, overlay_value) in overlay {
                let merged_value = match base.remove(&key) {
                    Some(base_value) => deep_merge(base_value, overlay_value),
                    None => overlay_value,
                };
                base.insert(key, merged_value);
            }
            toml::Value::Table(base)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use toml::Value;

    use super::deep_merge;

    #[test]
    fn deep_merge_tables_recurse_per_key() {
        // Given: 共通の入れ子テーブルを持つベースとオーバーレイ
        let base: Value = toml::from_str(
            r#"
            [routing]
            timeout = 30
            retries = 2
            [routing.headers]
            accept = "application/json"
            "#,
        )
        .expect("ベース TOML を解析できる");
        let overlay: Value = toml::from_str(
            r#"
            [routing]
            timeout = 60
            [routing.headers]
            authorization = "Bearer token"
            "#,
        )
        .expect("オーバーレイ TOML を解析できる");

        // When: 深マージする
        let merged = deep_merge(base, overlay);

        // Then: テーブルはキーごとに再帰マージされ、オーバーレイが優先される
        let expected: Value = toml::from_str(
            r#"
            [routing]
            timeout = 60
            retries = 2
            [routing.headers]
            accept = "application/json"
            authorization = "Bearer token"
            "#,
        )
        .expect("期待値 TOML を解析できる");
        assert_eq!(merged, expected);
    }

    #[test]
    fn deep_merge_scalar_replaced_by_overlay() {
        // Given: 同じキーに異なるスカラー値を持つベースとオーバーレイ
        let base: Value = toml::from_str("timeout = 30").expect("ベース TOML を解析できる");
        let overlay: Value =
            toml::from_str("timeout = 60").expect("オーバーレイ TOML を解析できる");

        // When: 深マージする
        let merged = deep_merge(base, overlay);

        // Then: オーバーレイのスカラー値で置き換わる
        let expected: Value = toml::from_str("timeout = 60").expect("期待値 TOML を解析できる");
        assert_eq!(merged, expected);
    }

    #[test]
    fn deep_merge_array_replaced_wholesale() {
        // Given: 同じキーに配列を持つベースとオーバーレイ
        let base: Value =
            toml::from_str("hosts = [\"base-a\", \"base-b\"]").expect("ベース TOML を解析できる");
        let overlay: Value =
            toml::from_str("hosts = [\"overlay-a\"]").expect("オーバーレイ TOML を解析できる");

        // When: 深マージする
        let merged = deep_merge(base, overlay);

        // Then: 配列は連結されず、オーバーレイの配列で全体置換される
        let expected: Value =
            toml::from_str("hosts = [\"overlay-a\"]").expect("期待値 TOML を解析できる");
        assert_eq!(merged, expected);
    }

    #[test]
    fn deep_merge_keeps_base_keys_missing_in_overlay() {
        // Given: ベースにしか存在しないキーを持つベースとオーバーレイ
        let base: Value = toml::from_str("host = \"base.example\"\ntimeout = 30")
            .expect("ベース TOML を解析できる");
        let overlay: Value =
            toml::from_str("timeout = 60").expect("オーバーレイ TOML を解析できる");

        // When: 深マージする
        let merged = deep_merge(base, overlay);

        // Then: オーバーレイにないベースのキーは維持される
        let expected: Value = toml::from_str("host = \"base.example\"\ntimeout = 60")
            .expect("期待値 TOML を解析できる");
        assert_eq!(merged, expected);
    }

    #[test]
    fn deep_merge_type_conflict_overlay_wins() {
        // Given: テーブルとスカラーの型が競合するベースとオーバーレイ
        let base: Value = toml::from_str(
            r#"
            table_to_scalar = { nested = "base" }
            scalar_to_table = "base"
            "#,
        )
        .expect("ベース TOML を解析できる");
        let overlay: Value = toml::from_str(
            r#"
            table_to_scalar = "overlay"
            scalar_to_table = { nested = "overlay" }
            "#,
        )
        .expect("オーバーレイ TOML を解析できる");

        // When: 深マージする
        let merged = deep_merge(base, overlay);

        // Then: 型が競合する場合はオーバーレイの値と型で全体置換される
        let expected: Value = toml::from_str(
            r#"
            table_to_scalar = "overlay"
            scalar_to_table = { nested = "overlay" }
            "#,
        )
        .expect("期待値 TOML を解析できる");
        assert_eq!(merged, expected);
    }
}
