//! プロバイダ設定の安全な書き戻し。

use std::io::{ErrorKind, Write};
use std::path::Path;

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

use crate::types::provider::ModelEntryConfig;
use crate::{CURRENT_VERSION, Config, ConfigError};

/// 秘密値ではなく資格情報の参照先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCredentialInput {
    Env { var: String },
    Keyring { service: String, account: String },
}

/// OpenAI 互換プロバイダの保存入力。
pub struct OpenAiCompatibleProviderInput {
    /// 保存先のプロファイル名。
    pub name: String,
    /// HTTP または HTTPS のベース URL (前後の空白は除去する)。
    pub base_url: String,
    /// 秘密値ではなく、その参照先の環境変数名。
    pub credential: ProviderCredentialInput,
    /// モデル ID 一覧 (空白・空要素・重複は除去する)。
    pub models: Vec<ModelEntryConfig>,
    /// 除外するモデル ID の一覧 (空白・空要素・重複は除去する)。
    pub excluded_models: Vec<String>,
    /// 正規化済みモデル一覧に含まれる既定モデル ID。
    pub default_model: String,
}

/// 保存入力を I/O なしで、名前・URL・認証参照・モデルの順に検証する。
///
/// # Errors
/// 最初の不正フィールドを [`ConfigError::InvalidField`] として返す。
pub fn validate_openai_compatible_provider_input(
    input: &OpenAiCompatibleProviderInput,
) -> Result<(), ConfigError> {
    let invalid =
        |field, message| invalid_field(&format!("providers.{}.{field}", input.name), message);
    if input.name.is_empty()
        || !input
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(invalid("name", "name must match ^[A-Za-z0-9_-]+$"));
    }
    let base_url = input.base_url.trim();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(invalid(
            "base_url",
            "base_url must start with http:// or https://",
        ));
    }
    match &input.credential {
        ProviderCredentialInput::Env { var } => {
            let mut bytes = var.bytes();
            if !bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_uppercase() || byte == b'_')
                || !bytes
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return Err(invalid(
                    "api_key_env",
                    "api_key_env must be an environment variable NAME matching ^[A-Z_][A-Z0-9_]*$ (never the API key itself; plaintext credentials are rejected per ADR 0008)",
                ));
            }
        }
        ProviderCredentialInput::Keyring { service, account } => {
            if service.trim().is_empty() {
                return Err(invalid(
                    "credential.service",
                    "keyring service must not be empty",
                ));
            }
            if account.trim().is_empty() {
                return Err(invalid(
                    "credential.account",
                    "keyring account must not be empty",
                ));
            }
        }
    }
    let models = normalized_models(&input.models);
    if !models.iter().any(|model| model.enabled) {
        return Err(invalid(
            "models",
            "models must contain at least one enabled non-empty model ID",
        ));
    }
    if !models
        .iter()
        .any(|model| model.enabled && model.id == input.default_model)
    {
        return Err(invalid(
            "default_model",
            "default_model must be a member of enabled models",
        ));
    }
    Ok(())
}

/// 対象プロファイルだけを sugar 形式で全置換し、検証後に原子的に保存する。
///
/// 既存ファイルは v2 のみ受理し、バージョン省略時は v2 を追記する。
/// 同一パスへの並行保存は呼び出し側で直列化する。
///
/// # Errors
/// 入力・TOML・設定の不正、非対応バージョン、入出力失敗を返す。
/// 保存前の検証に失敗した場合は既存ファイルに触れない。
pub fn save_openai_compatible_provider(
    path: &Path,
    input: &OpenAiCompatibleProviderInput,
) -> Result<(), ConfigError> {
    validate_openai_compatible_provider_input(input)?;
    let mut doc = read_document(path)?;

    let mut profile = Table::new();
    profile.insert("type", value("openai-compatible"));
    profile.insert("base_url", value(input.base_url.trim()));
    match &input.credential {
        ProviderCredentialInput::Env { var } => {
            profile.insert("api_key_env", value(var.as_str()));
        }
        ProviderCredentialInput::Keyring { service, account } => {
            let mut reference = InlineTable::new();
            reference.insert("type", "keyring".into());
            reference.insert("service", service.as_str().into());
            reference.insert("account", account.as_str().into());
            profile.insert("credential", value(reference));
        }
    }
    profile.insert(
        "models",
        value(
            normalized_models(&input.models)
                .into_iter()
                .map(|model| {
                    Ok(
                        if model.enabled
                            && model.metadata_source.is_none()
                            && model.metadata_ref.is_none()
                            && model.preset.is_none()
                            && model.context_window.is_none()
                            && model.pricing_for(None).is_none()
                        {
                            toml_edit::Value::from(model.id)
                        } else {
                            let mut table = InlineTable::new();
                            table.insert("id", model.id.into());
                            table.insert("enabled", model.enabled.into());
                            if let Some(source) = model.metadata_source {
                                let source = match source {
                                    crate::MetadataSource::Manual => "manual",
                                    crate::MetadataSource::ModelsDev => "models-dev",
                                    crate::MetadataSource::ProviderDefault => "provider-default",
                                };
                                table.insert("metadata_source", source.into());
                            }
                            if let Some(reference) = model.metadata_ref {
                                table.insert("metadata_ref", reference.into());
                            }
                            if let Some(preset) = model.preset {
                                table.insert("preset", preset.into());
                            }
                            if let Some(window) = model.context_window {
                                table.insert(
                                    "context_window",
                                    toml_edit::Value::from(i64::try_from(window).map_err(
                                        |_| {
                                            invalid_field(
                                                "context_window",
                                                "must fit in a TOML integer",
                                            )
                                        },
                                    )?),
                                );
                            }
                            for (key, price) in [
                                ("input_price", model.input_price),
                                ("output_price", model.output_price),
                                ("cache_read_price", model.cache_read_price),
                                ("cache_write_price", model.cache_write_price),
                            ] {
                                if let Some(price) = price {
                                    table.insert(key, price.into());
                                }
                            }
                            table.into()
                        },
                    )
                })
                .collect::<Result<Array, ConfigError>>()?,
        ),
    );
    let excluded_models = normalized_string_models(&input.excluded_models);
    if !excluded_models.is_empty() {
        profile.insert(
            "excluded_models",
            value(excluded_models.into_iter().collect::<Array>()),
        );
    }
    profile.insert("default_model", value(input.default_model.as_str()));
    insert_profile(&mut doc, &input.name, profile)?;
    write_document(path, &doc)
}

pub(crate) fn read_document(path: &Path) -> Result<DocumentMut, ConfigError> {
    let mut doc = match std::fs::read_to_string(path) {
        Ok(text) => text.parse::<DocumentMut>().map_err(|_| {
            invalid_field(
                &path.display().to_string(),
                "file is not valid TOML; fix it before saving",
            )
        })?,
        Err(error) if error.kind() == ErrorKind::NotFound => DocumentMut::new(),
        Err(error) => return Err(error.into()),
    };
    match doc.get("version") {
        Some(raw) => {
            let found = raw
                .as_integer()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| {
                    invalid_field("version", "version must be a non-negative u32 integer")
                })?;
            if found != CURRENT_VERSION {
                return Err(ConfigError::UnsupportedVersion {
                    found,
                    current: CURRENT_VERSION,
                });
            }
        }
        None => {
            doc.insert("version", value(i64::from(CURRENT_VERSION)));
        }
    }

    Ok(doc)
}

pub(crate) fn insert_profile(
    doc: &mut DocumentMut,
    name: &str,
    profile: Table,
) -> Result<(), ConfigError> {
    let providers = doc.entry("providers").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    let providers = providers
        .as_table_like_mut()
        .ok_or_else(|| invalid_field("providers", "providers must be a table"))?;
    providers.insert(name, Item::Table(profile));
    Ok(())
}

pub(crate) fn write_document(path: &Path, doc: &DocumentMut) -> Result<(), ConfigError> {
    let text = doc.to_string();
    let table = toml::from_str::<toml::value::Table>(&text).map_err(|error| {
        ConfigError::Migration(format!("failed to parse config before saving: {error}"))
    })?;
    let checked = crate::migrate::run(toml::Value::Table(table))?;
    crate::strict::validate_strict(&checked)?;
    let _: Config = checked.try_into().map_err(|error| {
        ConfigError::Migration(format!(
            "failed to deserialize config before saving: {error}"
        ))
    })?;

    let mut temporary_path = path.as_os_str().to_os_string();
    temporary_path.push(".tmp");
    let temporary_path = Path::new(&temporary_path);
    // 既存の一時ファイル (シンボリックリンクを含む) は上書きしない。
    let mut temporary = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary_path)?;
    temporary.write_all(text.as_bytes())?;
    #[cfg(unix)]
    temporary.sync_all()?;
    drop(temporary);
    std::fs::rename(temporary_path, path)?;
    Ok(())
}

pub(crate) fn invalid_field(path: &str, message: &str) -> ConfigError {
    ConfigError::InvalidField {
        path: path.to_owned(),
        message: message.to_owned(),
    }
}

pub(crate) fn normalized_models(input: &[ModelEntryConfig]) -> Vec<ModelEntryConfig> {
    let mut models: Vec<ModelEntryConfig> = Vec::new();
    for model in input {
        let id = model.id.trim();
        if id.is_empty() {
            continue;
        }
        if let Some(existing) = models.iter_mut().find(|entry| entry.id == id) {
            existing.enabled |= model.enabled;
        } else {
            models.push(ModelEntryConfig {
                id: id.into(),
                ..model.clone()
            });
        }
    }
    models
}

fn normalized_string_models(input: &[String]) -> Vec<&str> {
    let mut models = Vec::new();
    for model in input {
        let model = model.trim();
        if !model.is_empty() && !models.contains(&model) {
            models.push(model);
        }
    }
    models
}
