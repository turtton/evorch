use super::ToolExecutor;

/// 登録ツールから導出する、プロバイダー非依存のモデル向け定義。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl ToolExecutor {
    /// 登録済みツールの実スキーマと説明を名前順で返す。
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<_> = self
            .tools
            .values()
            .map(|registered| ToolSpec {
                name: registered.tool.name().to_owned(),
                description: registered.tool.description().to_owned(),
                input_schema: registered.tool.schema(),
            })
            .collect();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }
}
