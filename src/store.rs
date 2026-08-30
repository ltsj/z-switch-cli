//! providers.json 数据模型 + 读写。
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::config;

fn find_toml_string(value: &toml::Value, key: &str) -> Option<String> {
    let table = value.as_table()?;
    if let Some(found) = table
        .get(key)
        .and_then(toml::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Some(found.to_string());
    }
    table.values().find_map(|value| {
        if value.is_table() {
            find_toml_string(value, key)
        } else {
            None
        }
    })
}

/// 从 TOML 中读取指定字符串键，兼容 Codex 的嵌套 provider 表。
/// 旧配置若无法整体解析，则退回到严格的行级键匹配。
pub fn extract_toml_string(config_text: &str, key: &str) -> Option<String> {
    if let Ok(document) = config_text.parse::<toml::Value>() {
        if let Some(value) = find_toml_string(&document, key) {
            return Some(value);
        }
    }
    config_text.lines().find_map(|line| {
        let (raw_key, raw_value) = line.trim().split_once('=')?;
        if raw_key.trim() != key {
            return None;
        }
        parse_toml_string_value(raw_value)
    })
}

/// Parse a TOML string value from the right-hand side of a key/value line.
/// This is intentionally strict: it does not treat numbers, booleans, or
/// inline comments inside an unquoted value as strings.
pub fn parse_toml_string_value(raw_value: &str) -> Option<String> {
    let value_text = raw_value.trim();
    toml::from_str::<toml::Value>(&format!("value = {value_text}"))
        .ok()
        .and_then(|value| {
            value
                .get("value")
                .and_then(toml::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
}

/// Read a TOML table header while accepting an inline comment.
///
/// For example, `[model_providers.custom] # relay` returns
/// `model_providers.custom`. Array-of-tables (`[[...]]`) are deliberately
/// excluded because the configuration rewriting code only operates on
/// ordinary tables.
pub fn normalized_toml_section(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("[[") || !trimmed.starts_with('[') {
        return None;
    }

    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in trimmed.char_indices().skip(1) {
        if let Some(current_quote) = quote {
            if current_quote == '"' && escaped {
                escaped = false;
            } else if current_quote == '"' && ch == '\\' {
                escaped = true;
            } else if ch == current_quote {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == ']' {
            let trailing = trimmed[index + ch.len_utf8()..].trim();
            if !trailing.is_empty() && !trailing.starts_with('#') {
                return None;
            }
            return Some(trimmed[1..index].replace(['"', '\''], ""));
        }
    }
    None
}

/// Return whether a line starts an array-of-tables section (`[[...]]`).
///
/// `normalized_toml_section` intentionally returns only ordinary table
/// headers. Callers doing line-oriented fallback parsing still need to reset
/// their section state when an array table begins; otherwise fields inside the
/// array can be attributed to the preceding ordinary table.
pub fn is_toml_array_section(line: &str) -> bool {
    line.trim().starts_with("[[")
}

/// 从 Codex 完整配置中读取当前 model_provider 对应表里的字段。
/// 完整 config.toml 可能同时保存多个 provider，不能无条件取第一个嵌套表。
pub fn extract_codex_provider_string(config_text: &str, key: &str) -> Option<String> {
    let selected = extract_codex_provider_id(config_text);
    if let Ok(document) = config_text.parse::<toml::Value>() {
        let root = document.as_table()?;
        if let Some(selected) = selected.as_deref() {
            if let Some(provider) = root
                .get("model_providers")
                .and_then(|providers| providers.get(selected))
            {
                return provider
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string);
            }

            // Older Codex configs can select a provider while keeping its
            // fields at the document root. Only use the root fallback when
            // the selected provider table is absent; never borrow a value
            // from an incomplete selected table or another provider.
            return root
                .get(key)
                .and_then(toml::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string);
        }
    }

    // 没有选择 provider 时，兼容旧配置里的根级字段或第一个可见字段。
    // 一旦存在明确的 model_provider，上面的分支已经返回 None；不能在这里
    // 递归到其它 provider，否则会把未选中的中转地址/协议借给当前配置。
    if let Some(selected) = selected.as_deref() {
        if codex_provider_section_exists(config_text, selected) {
            return None;
        }
        return extract_root_toml_string(config_text, key);
    }
    extract_toml_string(config_text, key)
}

fn extract_root_toml_string(config_text: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    config_text.lines().find_map(|line| {
        if is_toml_array_section(line) {
            in_section = true;
            return None;
        }
        if normalized_toml_section(line).is_some() {
            in_section = true;
            return None;
        }
        if in_section {
            return None;
        }
        let (raw_key, raw_value) = line.trim().split_once('=')?;
        (raw_key.trim() == key)
            .then(|| parse_toml_string_value(raw_value))
            .flatten()
    })
}

/// 判断 Codex 选中的 provider 是否有对应的嵌套表。
///
/// 该信息用于区分“嵌套表字段缺失”和“旧版根级配置”，两者不能使用
/// 相同的回退策略。
pub fn codex_provider_section_exists(config_text: &str, provider: &str) -> bool {
    let wanted = format!("model_providers.{provider}");
    if let Ok(document) = config_text.parse::<toml::Value>() {
        return document
            .as_table()
            .and_then(|root| root.get("model_providers"))
            .and_then(toml::Value::as_table)
            .is_some_and(|providers| providers.contains_key(provider));
    }
    config_text.lines().any(|line| {
        normalized_toml_section(line).is_some_and(|section| {
            section == wanted || section.starts_with(&(wanted.clone() + "."))
        })
    })
}

/// Read Codex's selected provider id from the document root only.
///
/// This is separate from `extract_toml_string`: provider selection controls
/// which table may be rewritten, so a same-named key in another TOML section
/// must never influence it.
pub fn extract_codex_provider_id(config_text: &str) -> Option<String> {
    if let Ok(document) = config_text.parse::<toml::Value>() {
        if let Some(value) = document
            .as_table()?
            .get("model_provider")
            .and_then(toml::Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            return Some(value.to_string());
        }
    }

    let mut section = None;
    config_text.lines().find_map(|line| {
        if is_toml_array_section(line) {
            // An array table is not the document root. Keep a non-None
            // sentinel so fields inside it cannot be mistaken for root keys.
            section = Some(String::new());
            return None;
        }
        if let Some(header) = normalized_toml_section(line) {
            section = Some(header);
            return None;
        }
        if section.is_some() {
            return None;
        }
        let (raw_key, raw_value) = line.trim().split_once('=')?;
        (raw_key.trim() == "model_provider")
            .then(|| parse_toml_string_value(raw_value))
            .flatten()
    })
}

/// Read a Grok endpoint field only from the document root or `[endpoints]`.
/// A generic recursive TOML lookup is unsafe here because model entries may
/// contain their own `base_url` or credentials for a different model.
pub fn extract_grok_endpoint_string(config_text: &str, key: &str) -> Option<String> {
    if let Ok(document) = config_text.parse::<toml::Value>() {
        let root = document.as_table()?;
        return root
            .get(key)
            .and_then(toml::Value::as_str)
            .or_else(|| {
                root.get("endpoints")
                    .and_then(toml::Value::as_table)
                    .and_then(|endpoints| endpoints.get(key))
                    .and_then(toml::Value::as_str)
            })
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
    }

    let mut section = None;
    config_text.lines().find_map(|line| {
        if is_toml_array_section(line) {
            section = Some(String::new());
            return None;
        }
        if let Some(header) = normalized_toml_section(line) {
            section = Some(header);
            return None;
        }
        let (raw_key, raw_value) = line.trim().split_once('=')?;
        let in_scope = section
            .as_deref()
            .is_none_or(|current| current == "endpoints");
        (in_scope && raw_key.trim() == key).then(|| parse_toml_string_value(raw_value))?
    })
}

fn remove_toml_dotted_key(
    table: &mut toml::map::Map<String, toml::Value>,
    key: &str,
) -> Option<toml::Value> {
    if let Some(value) = table.remove(key) {
        return Some(value);
    }
    let (head, tail) = key.split_once('.')?;
    table
        .get_mut(head)
        .and_then(toml::Value::as_table_mut)
        .and_then(|nested| remove_toml_dotted_key(nested, tail))
}

/// 生成 TOML basic string，避免命令行输入破坏 Codex/Grok 配置格式。
pub fn quote_toml_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{08}' => quoted.push_str("\\b"),
            '\u{0C}' => quoted.push_str("\\f"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(quoted, "\\u{:04x}", c as u32);
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// 按 GUI providerFactory 的协议生成 Codex 配置。
pub fn build_codex_config(name: &str, base_url: &str, model: &str, wire_api: &str) -> String {
    build_codex_config_with_options(name, base_url, model, wire_api, "high", true, false, None)
}

#[allow(clippy::too_many_arguments)]
fn build_codex_config_with_options(
    name: &str,
    base_url: &str,
    model: &str,
    wire_api: &str,
    reasoning_effort: &str,
    disable_response_storage: bool,
    requires_openai_auth: bool,
    context_window: Option<u64>,
) -> String {
    let context_window = context_window
        .filter(|value| *value > 0)
        .map(|value| format!("model_context_window = {value}\n"))
        .unwrap_or_default();
    format!(
        "model_provider = \"custom\"\nmodel = {}\nmodel_reasoning_effort = {}\ndisable_response_storage = {}\n{}\n[model_providers.custom]\nname = {}\nbase_url = {}\nwire_api = {}\nrequires_openai_auth = {}\n",
        quote_toml_string(model),
        quote_toml_string(reasoning_effort),
        disable_response_storage,
        context_window,
        quote_toml_string(name),
        quote_toml_string(base_url),
        quote_toml_string(wire_api),
        requires_openai_auth,
    )
}

/// 重建 Codex 配置时保留 GUI 表单支持的高级选项，避免 CLI 编辑基础字段
/// 时悄悄重置 reasoning、响应存储、认证和上下文窗口设置。
pub fn build_codex_config_preserving(
    existing: &str,
    name: &str,
    base_url: &str,
    model: &str,
    wire_api: &str,
) -> String {
    if let Some(config) = preserve_codex_config(existing, name, base_url, model, wire_api) {
        return config;
    }

    let reasoning = extract_toml_string(existing, "model_reasoning_effort")
        .unwrap_or_else(|| "high".to_string());
    let disable_storage = extract_toml_bool(existing, "disable_response_storage").unwrap_or(true);
    let requires_auth = extract_toml_bool(existing, "requires_openai_auth").unwrap_or(false);
    let context_window = extract_toml_integer(existing, "model_context_window")
        .or_else(|| extract_toml_integer(existing, "context_window"));
    build_codex_config_with_options(
        name,
        base_url,
        model,
        wire_api,
        &reasoning,
        disable_storage,
        requires_auth,
        context_window,
    )
}

/// 只更新 CLI 表单负责的 Codex 字段，保留 MCP、沙箱、历史等其它 TOML 配置。
/// 解析失败时由调用方回退到兼容旧配置的重建逻辑。
fn preserve_codex_config(
    existing: &str,
    name: &str,
    base_url: &str,
    model: &str,
    wire_api: &str,
) -> Option<String> {
    let mut document = existing.parse::<toml::Value>().ok()?;
    let root = document.as_table_mut()?;

    let provider_id = root
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("custom")
        .to_string();
    root.insert(
        "model_provider".into(),
        toml::Value::String(provider_id.clone()),
    );
    root.insert("model".into(), toml::Value::String(model.to_string()));
    root.entry("model_reasoning_effort")
        .or_insert_with(|| toml::Value::String("high".into()));
    root.entry("disable_response_storage")
        .or_insert(toml::Value::Boolean(true));

    if !matches!(root.get("model_providers"), Some(toml::Value::Table(_))) {
        root.insert(
            "model_providers".into(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }
    let providers = root
        .get_mut("model_providers")
        .and_then(toml::Value::as_table_mut)?;
    if !matches!(providers.get(&provider_id), Some(toml::Value::Table(_))) {
        providers.insert(
            provider_id.clone(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }
    let provider = providers
        .get_mut(&provider_id)
        .and_then(toml::Value::as_table_mut)?;
    provider.insert("name".into(), toml::Value::String(name.to_string()));
    provider.insert("base_url".into(), toml::Value::String(base_url.to_string()));
    provider.insert("wire_api".into(), toml::Value::String(wire_api.to_string()));
    provider
        .entry("requires_openai_auth")
        .or_insert(toml::Value::Boolean(false));

    toml::to_string(&document).ok()
}

/// 按 GUI providerFactory 的协议生成 Grok 配置。Grok 的 Key 必须写在
/// config.toml 的模型表里，单独放进 settingsConfig.auth 不会进入 live 文件。
pub fn build_grok_config(
    name: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    api_backend: &str,
) -> String {
    build_grok_config_with_context(name, base_url, api_key, model, api_backend, 500000)
}

fn build_grok_config_with_context(
    name: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    api_backend: &str,
    context_window: u64,
) -> String {
    let model = if model.trim().is_empty() {
        "grok-4.5"
    } else {
        model.trim()
    };
    format!(
        "[models]\ndefault = {}\nweb_search = {}\n\n[endpoints]\nmodels_base_url = {}\n\n[model.{}]\nmodel = {}\nname = {}\ndescription = {}\napi_key = {}\napi_backend = {}\ncontext_window = {context_window}\n",
        quote_toml_string(model),
        quote_toml_string(model),
        quote_toml_string(base_url),
        quote_toml_string(model),
        quote_toml_string(model),
        quote_toml_string(name),
        quote_toml_string(name),
        quote_toml_string(api_key),
        quote_toml_string(api_backend),
    )
}

/// 重建 Grok 配置时保留已有上下文窗口设置。
pub fn build_grok_config_preserving(
    existing: &str,
    name: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    api_backend: &str,
) -> String {
    if let Some(config) =
        preserve_grok_config(existing, name, base_url, api_key, model, api_backend)
    {
        return config;
    }

    let context_window = extract_toml_integer(existing, "context_window")
        .filter(|value| *value > 0)
        .unwrap_or(500000);
    build_grok_config_with_context(name, base_url, api_key, model, api_backend, context_window)
}

/// 更新 Grok 当前默认模型和端点，同时保留其它模型与自定义配置段落。
fn preserve_grok_config(
    existing: &str,
    name: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    api_backend: &str,
) -> Option<String> {
    let mut document = existing.parse::<toml::Value>().ok()?;
    let root = document.as_table_mut()?;

    if !matches!(root.get("models"), Some(toml::Value::Table(_))) {
        root.insert("models".into(), toml::Value::Table(toml::map::Map::new()));
    }
    let models = root.get_mut("models").and_then(toml::Value::as_table_mut)?;
    let old_default = models
        .get("default")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let selected_model = if model.trim().is_empty() {
        old_default
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "grok-4.5".to_string())
    } else {
        model.trim().to_string()
    };
    models.insert(
        "default".into(),
        toml::Value::String(selected_model.clone()),
    );
    if models
        .get("web_search")
        .and_then(toml::Value::as_str)
        .is_none_or(|value| old_default.as_deref() == Some(value))
    {
        models.insert(
            "web_search".into(),
            toml::Value::String(selected_model.clone()),
        );
    }

    if !matches!(root.get("endpoints"), Some(toml::Value::Table(_))) {
        root.insert(
            "endpoints".into(),
            toml::Value::Table(toml::map::Map::new()),
        );
    }
    let endpoints = root
        .get_mut("endpoints")
        .and_then(toml::Value::as_table_mut)?;
    endpoints.insert(
        "models_base_url".into(),
        toml::Value::String(base_url.to_string()),
    );

    if !matches!(root.get("model"), Some(toml::Value::Table(_))) {
        root.insert("model".into(), toml::Value::Table(toml::map::Map::new()));
    }
    let model_table = root.get_mut("model").and_then(toml::Value::as_table_mut)?;
    // If the new default already has a model table, update that table in
    // place. Reusing the old default's table here would silently overwrite
    // the existing model's credentials and provider-specific options.
    let existing_selected = model_table
        .get(&selected_model)
        .and_then(toml::Value::as_table)
        .cloned();
    let mut model_config = match existing_selected {
        Some(table) => table,
        None => match old_default
            .as_deref()
            .and_then(|id| remove_toml_dotted_key(model_table, id))
        {
            Some(toml::Value::Table(table)) => table,
            _ => toml::map::Map::new(),
        },
    };
    let context_window = model_config
        .get("context_window")
        .and_then(toml::Value::as_integer)
        .filter(|value| *value > 0)
        .unwrap_or(500000);
    model_config.insert("model".into(), toml::Value::String(selected_model.clone()));
    model_config.insert("name".into(), toml::Value::String(name.to_string()));
    model_config.insert("description".into(), toml::Value::String(name.to_string()));
    model_config.insert("api_key".into(), toml::Value::String(api_key.to_string()));
    model_config.insert(
        "api_backend".into(),
        toml::Value::String(api_backend.to_string()),
    );
    model_config.insert(
        "context_window".into(),
        toml::Value::Integer(context_window),
    );
    model_table.insert(selected_model, toml::Value::Table(model_config));

    toml::to_string(&document).ok()
}

/// 读取 Grok 当前默认模型对应表中的字段。
///
/// 原项目使用 `[model."<model id>"]`，不能直接在整个 TOML 文档中搜索
/// `api_key`，否则多模型配置会拿到其它模型的凭据。对早期未加引号、被
/// TOML 解析成 dotted key 的配置也保留兼容读取。
pub fn extract_grok_model_string(config_text: &str, key: &str) -> Option<String> {
    if let Ok(document) = config_text.parse::<toml::Value>() {
        let root = document.as_table()?;
        let selected = root
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(toml::Value::as_str);
        if let Some(selected) = selected {
            let selected_value = root
                .get("model")
                .and_then(toml::Value::as_table)
                .and_then(|model_table| {
                    model_table
                        .get(selected)
                        .or_else(|| get_toml_dotted_key(model_table, selected))
                })
                .and_then(|model_config| model_config.get(key))
                .and_then(toml::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string);
            return selected_value.or_else(|| extract_grok_explicit_string(root, key));
        }

        // Older files may contain exactly one model table but no
        // `models.default`. It is safe to read that table; with multiple
        // model tables there is no unambiguous credential to select.
        if let Some(model_table) = root.get("model").and_then(toml::Value::as_table) {
            let candidates: Vec<&toml::Value> = model_table
                .values()
                .filter(|value| value.is_table())
                .collect();
            if candidates.len() == 1 {
                if let Some(value) = candidates[0]
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                {
                    return Some(value);
                }
            }
        }

        return extract_grok_explicit_string(root, key);
    }

    // A malformed legacy file is still parsed section-by-section. Do not use
    // the generic recursive fallback here because it can borrow a value from
    // an unrelated model section.
    let mut section = None;
    let mut selected_model = None;
    let mut model_sections = Vec::new();
    let mut values = Vec::new();
    for line in config_text.lines() {
        if is_toml_array_section(line) {
            // Do not let an array-of-tables inherit the preceding model
            // section in malformed legacy TOML.
            section = Some(String::new());
            continue;
        }
        if let Some(header) = normalized_toml_section(line) {
            section = Some(header.clone());
            if header == "models" {
                continue;
            }
            if header.starts_with("model.") {
                model_sections.push(header);
            }
            continue;
        }
        let Some((raw_key, raw_value)) = line.trim().split_once('=') else {
            continue;
        };
        let Some(value) = parse_toml_string_value(raw_value) else {
            continue;
        };
        match section.as_deref() {
            Some("models") if raw_key.trim() == "default" => selected_model = Some(value),
            Some(current) if current.starts_with("model.") && raw_key.trim() == key => {
                values.push((current.to_string(), value));
            }
            _ => {}
        }
    }
    if let Some(selected) = selected_model {
        let wanted = format!("model.{selected}");
        if let Some((_, value)) = values.into_iter().find(|(section, _)| section == &wanted) {
            return Some(value);
        }
    } else if model_sections.len() == 1 {
        return values.into_iter().next().map(|(_, value)| value);
    }
    None
}

fn extract_grok_explicit_string(
    root: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Option<String> {
    root.get(key)
        .and_then(toml::Value::as_str)
        .or_else(|| {
            root.get("endpoints")
                .and_then(toml::Value::as_table)
                .and_then(|endpoints| endpoints.get(key))
                .and_then(toml::Value::as_str)
        })
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

pub fn extract_grok_default_model(config_text: &str) -> Option<String> {
    if let Ok(document) = config_text.parse::<toml::Value>() {
        return document
            .get("models")
            .and_then(|models| models.get("default"))
            .and_then(toml::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
    }
    // The fallback is used only for malformed legacy files.  A line-level
    // search must still respect TOML sections; otherwise an unrelated
    // `[other] default = ...` can be mistaken for `[models].default`.
    let mut section = None;
    config_text.lines().find_map(|line| {
        if is_toml_array_section(line) {
            section = Some(String::new());
            return None;
        }
        if let Some(header) = normalized_toml_section(line) {
            section = Some(header);
            return None;
        }
        if section.as_deref() != Some("models") {
            return None;
        }
        let (raw_key, raw_value) = line.trim().split_once('=')?;
        if raw_key.trim() != "default" {
            return None;
        }
        parse_toml_string_value(raw_value)
    })
}

fn get_toml_dotted_key<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
) -> Option<&'a toml::Value> {
    if let Some(value) = table.get(key) {
        return Some(value);
    }
    let (head, tail) = key.split_once('.')?;
    table
        .get(head)
        .and_then(toml::Value::as_table)
        .and_then(|nested| get_toml_dotted_key(nested, tail))
}

fn extract_toml_bool(config_text: &str, key: &str) -> Option<bool> {
    config_text.lines().find_map(|line| {
        let (raw_key, raw_value) = line.trim().split_once('=')?;
        if raw_key.trim() != key {
            return None;
        }
        raw_value
            .split('#')
            .next()
            .and_then(|value| value.trim().parse::<bool>().ok())
    })
}

fn extract_toml_integer(config_text: &str, key: &str) -> Option<u64> {
    config_text.lines().find_map(|line| {
        let (raw_key, raw_value) = line.trim().split_once('=')?;
        if raw_key.trim() != key {
            return None;
        }
        raw_value
            .split('#')
            .next()
            .and_then(|value| value.trim().parse::<u64>().ok())
    })
}

pub const CLAUDE_OFFICIAL_PROVIDER_ID: &str = "claude-official";
pub const CODEX_OFFICIAL_PROVIDER_ID: &str = "codex-official";

pub fn official_provider_id(app: &str) -> Option<&'static str> {
    match app {
        "claude" => Some(CLAUDE_OFFICIAL_PROVIDER_ID),
        "codex" => Some(CODEX_OFFICIAL_PROVIDER_ID),
        _ => None,
    }
}

/// 判断供应商是否是指定应用对应的官方卡片。
///
/// `providers.json` 是可导入、可被 GUI 修改的共享文件，不能只按 ID 判断：
/// 一个错误归档到 Claude 下的 `codex-official` 不应被当成 Claude 官方账号。
pub fn is_official_provider_for_app(app: &str, provider: &Provider) -> bool {
    official_provider_id(app).is_some_and(|id| provider.id == id)
}

pub fn official_provider(app: &str) -> Provider {
    match app {
        "claude" => Provider {
            id: CLAUDE_OFFICIAL_PROVIDER_ID.into(),
            name: "Claude 官方账号".into(),
            category: Some("official".into()),
            settings_config: json!({ "env": {} }),
            meta: json!({
                "kind": "officialLocal",
                "system": true,
                "iconColor": "#D4915D"
            }),
            failover: json!({ "enabled": false }),
        },
        "codex" => Provider {
            id: CODEX_OFFICIAL_PROVIDER_ID.into(),
            name: "OpenAI 官方账号".into(),
            category: Some("official".into()),
            settings_config: json!({ "auth": {}, "config": "" }),
            meta: json!({
                "kind": "officialLocal",
                "system": true,
                "iconColor": "#10A37F",
                "wireApi": "responses"
            }),
            failover: json!({ "enabled": false }),
        },
        _ => unreachable!("unsupported official provider app"),
    }
}

/// 单个供应商。`settings_config` 是唯一按 app 类型分叉的字段：
/// Claude = `{ env: {...} }`；Codex = `{ auth: {...}, config: "toml" }`；Grok = `{ config: "toml" }`。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub category: Option<String>,
    /// 原样写进 live 配置文件的内容
    pub settings_config: Value,
    /// 图标 / apiKeyField / wireApi 等元数据（不写 live）
    #[serde(default)]
    pub meta: Value,
    /// 故障转移偏好
    #[serde(default)]
    pub failover: Value,
}

impl Provider {
    pub fn extract_base_url(&self, app: &str) -> Option<String> {
        match app {
            "claude" => self
                .settings_config
                .get("env")
                .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
                .and_then(|v| v.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(|s| s.to_string()),
            "codex" => {
                let cfg = self
                    .settings_config
                    .get("config")
                    .and_then(|v| v.as_str())?;
                extract_codex_provider_string(cfg, "base_url")
            }
            "grok" => {
                let cfg = self
                    .settings_config
                    .get("config")
                    .and_then(|v| v.as_str())?;
                extract_grok_endpoint_string(cfg, "models_base_url")
                    .or_else(|| extract_grok_endpoint_string(cfg, "base_url"))
            }
            _ => None,
        }
    }

    pub fn extract_api_key(&self, app: &str) -> Option<String> {
        match app {
            "claude" => {
                let env = self.settings_config.get("env")?;
                let preferred = self
                    .meta
                    .get("apiKeyField")
                    .and_then(Value::as_str)
                    .filter(|field| matches!(*field, "ANTHROPIC_AUTH_TOKEN" | "ANTHROPIC_API_KEY"))
                    .unwrap_or("ANTHROPIC_AUTH_TOKEN");
                let fallback = if preferred == "ANTHROPIC_API_KEY" {
                    "ANTHROPIC_AUTH_TOKEN"
                } else {
                    "ANTHROPIC_API_KEY"
                };
                [preferred, fallback]
                    .iter()
                    .find_map(|field| {
                        env.get(*field)
                            .and_then(Value::as_str)
                            .filter(|value| !value.trim().is_empty())
                    })
                    .map(str::to_string)
            }
            "codex" => self
                .settings_config
                .get("auth")
                .and_then(|a| a.get("OPENAI_API_KEY"))
                .and_then(|v| v.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(|s| s.to_string()),
            "grok" => {
                let auth_key = self.settings_config.get("auth").and_then(|auth| {
                    ["GROK_API_KEY", "XAI_API_KEY"].iter().find_map(|field| {
                        auth.get(*field)
                            .and_then(Value::as_str)
                            .filter(|value| !value.trim().is_empty())
                            .map(str::to_string)
                    })
                });
                if auth_key.is_some() {
                    return auth_key;
                }
                let cfg = self
                    .settings_config
                    .get("config")
                    .and_then(|v| v.as_str())?;
                extract_grok_model_string(cfg, "api_key")
                    .or_else(|| extract_grok_model_string(cfg, "grok_api_key"))
                    .or_else(|| extract_grok_model_string(cfg, "xai_api_key"))
            }
            _ => None,
        }
    }

    pub fn extract_model(&self, app: &str) -> Option<String> {
        match app {
            "claude" => {
                let env = self.settings_config.get("env")?;
                [
                    "ANTHROPIC_MODEL",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                    "ANTHROPIC_DEFAULT_FABLE_MODEL",
                ]
                .iter()
                .find_map(|field| {
                    env.get(*field)
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                })
                .map(str::to_string)
            }
            "codex" => {
                let cfg = self
                    .settings_config
                    .get("config")
                    .and_then(|v| v.as_str())?;
                extract_toml_string(cfg, "model")
            }
            "grok" => {
                let cfg = self
                    .settings_config
                    .get("config")
                    .and_then(|v| v.as_str())?;
                extract_grok_default_model(cfg).or_else(|| extract_grok_model_string(cfg, "model"))
            }
            _ => None,
        }
    }

    pub fn extract_wire_api(&self, app: &str) -> String {
        let metadata = self
            .meta
            .get("wireApi")
            .and_then(|v| v.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        if let Some(value) = metadata {
            return value;
        }
        let config_key = if app == "grok" {
            "api_backend"
        } else {
            "wire_api"
        };
        self.settings_config
            .get("config")
            .and_then(Value::as_str)
            .and_then(|config| {
                let extracted = if app == "codex" {
                    extract_codex_provider_string(config, config_key)
                } else if app == "grok" {
                    extract_grok_model_string(config, config_key)
                } else {
                    extract_toml_string(config, config_key)
                };
                extracted.or_else(|| {
                    if app == "grok" {
                        extract_grok_endpoint_string(config, "wire_api")
                    } else {
                        None
                    }
                })
            })
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "responses".to_string())
    }

    pub fn extract_api_key_field(&self, app: &str) -> Option<String> {
        if app == "claude" {
            let preferred = self
                .meta
                .get("apiKeyField")
                .and_then(Value::as_str)
                .filter(|field| matches!(*field, "ANTHROPIC_AUTH_TOKEN" | "ANTHROPIC_API_KEY"))
                .unwrap_or("ANTHROPIC_AUTH_TOKEN");
            let fallback = if preferred == "ANTHROPIC_API_KEY" {
                "ANTHROPIC_AUTH_TOKEN"
            } else {
                "ANTHROPIC_API_KEY"
            };
            let env = self.settings_config.get("env").and_then(Value::as_object);
            if env
                .and_then(|env| env.get(preferred))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
            {
                Some(preferred.to_string())
            } else if env
                .and_then(|env| env.get(fallback))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
            {
                Some(fallback.to_string())
            } else {
                Some(preferred.to_string())
            }
        } else {
            None
        }
    }
}

/// 单个工具（claude / codex / grok）的数据。
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AppData {
    /// 当前激活的 provider id（激活项不可删）
    #[serde(default)]
    pub current: Option<String>,
    /// 排序
    #[serde(default)]
    pub order: Vec<String>,
    /// id -> provider
    #[serde(default)]
    pub providers: HashMap<String, Provider>,
}

/// providers.json 根结构。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Root {
    pub version: u32,
    /// "claude" / "codex" / "grok"
    pub apps: HashMap<String, AppData>,
    /// 全局设置
    #[serde(default)]
    pub settings: Value,
}

fn default_settings() -> Value {
    json!({
        "theme": "light",
        "autoLaunch": false,
        "backupBeforeWrite": true,
        "initialImportDone": false,
        "grokImportDone": false,
        "applyClaudePlugin": false,
        "skipClaudeOnboarding": false,
        "applyClaudeDesktop": false,
        "reliability": {
            "proxyEnabled": false,
            "failoverEnabled": false,
            "circuitBreaker": true,
            "connectTimeoutSeconds": 10,
            "streamingFirstByteTimeoutSeconds": 60,
            "streamingIdleTimeoutSeconds": 120,
            "nonStreamingTimeoutSeconds": 600,
            "requestBodyLimitMb": 64,
            "poolMaxIdlePerHost": 10,
            "tcpKeepaliveSeconds": 60,
            "proxyErrorLogEnabled": true,
            "proxyErrorLogMaxMb": 5
        },
        "ztest": { "connected": false }
    })
}

fn normalize_app_data(data: &mut AppData, fallback_current: Option<&str>) -> bool {
    let mut changed = false;
    let original_order = data.order.clone();
    let mut seen = std::collections::HashSet::new();
    data.order
        .retain(|id| data.providers.contains_key(id) && seen.insert(id.clone()));
    if data.order != original_order {
        changed = true;
    }

    let ordered: std::collections::HashSet<_> = data.order.iter().collect();
    let mut missing: Vec<String> = data
        .providers
        .keys()
        .filter(|id| !ordered.contains(id))
        .cloned()
        .collect();
    missing.sort();
    if !missing.is_empty() {
        data.order.extend(missing);
        changed = true;
    }

    if data
        .current
        .as_ref()
        .is_some_and(|id| !data.providers.contains_key(id))
    {
        data.current = fallback_current.map(str::to_string);
        changed = true;
    }
    changed
}

impl Root {
    pub fn default_seeded() -> Self {
        let mut apps: HashMap<String, AppData> = HashMap::new();
        for app in ["claude", "codex"] {
            let provider = official_provider(app);
            let id = provider.id.clone();
            apps.insert(
                app.into(),
                AppData {
                    current: Some(id.clone()),
                    order: vec![id.clone()],
                    providers: HashMap::from([(id, provider)]),
                },
            );
        }
        apps.insert("grok".into(), AppData::default());
        Root {
            version: 3,
            apps,
            settings: default_settings(),
        }
    }

    /// 补齐不可删除的本机官方账号卡片，并清理历史多余字段。
    pub fn ensure_official_providers(&mut self) -> bool {
        let mut changed = false;
        if self.version < 3 {
            self.version = 3;
            changed = true;
        }

        if !self.apps.contains_key("grok") {
            self.apps.insert("grok".into(), AppData::default());
            changed = true;
        }

        if !self.settings.is_object() {
            self.settings = default_settings();
            changed = true;
        }

        // map key 是 current/order 的引用目标，provider.id 必须与它保持一致。
        // 外部导入的 JSON 若不一致，后续切换会把 current 写成悬空 ID。
        for data in self.apps.values_mut() {
            for (id, provider) in &mut data.providers {
                if provider.id != *id {
                    provider.id = id.clone();
                    changed = true;
                }
            }
        }

        for app in ["claude", "codex"] {
            let seed = official_provider(app);
            let official_id = seed.id.clone();
            let data = self.apps.entry(app.into()).or_default();

            match data.providers.get_mut(&official_id) {
                Some(existing) => {
                    if existing.name != seed.name {
                        existing.name = seed.name.clone();
                        changed = true;
                    }
                    if existing.category.as_deref() != Some("official") {
                        existing.category = Some("official".into());
                        changed = true;
                    }
                    let config = existing.settings_config.clone();
                    let normalized = if app == "claude" {
                        json!({ "env": config.get("env").cloned().unwrap_or_else(|| json!({})) })
                    } else {
                        json!({
                            "auth": {},
                            "config": config.get("config").and_then(Value::as_str).unwrap_or("")
                        })
                    };
                    if existing.settings_config != normalized {
                        existing.settings_config = normalized;
                        changed = true;
                    }
                    if existing.meta != seed.meta {
                        existing.meta = seed.meta.clone();
                        changed = true;
                    }
                    if existing.failover != seed.failover {
                        existing.failover = seed.failover.clone();
                        changed = true;
                    }
                }
                None => {
                    data.providers.insert(official_id.clone(), seed);
                    changed = true;
                }
            }

            if !data.order.contains(&official_id) {
                data.order.insert(0, official_id.clone());
                changed = true;
            }
        }

        for (app, data) in &mut self.apps {
            changed |= normalize_app_data(data, official_provider_id(app));
        }

        if let Some(data) = self.apps.get_mut("codex") {
            for provider in data.providers.values_mut() {
                if is_official_provider_for_app("codex", provider) {
                    continue;
                }
                let Some(root) = provider.settings_config.as_object_mut() else {
                    continue;
                };
                let key = root
                    .get("auth")
                    .and_then(Value::as_object)
                    .and_then(|auth| auth.get("OPENAI_API_KEY"))
                    .cloned();
                let sanitized = match key {
                    Some(key) => json!({ "OPENAI_API_KEY": key }),
                    None => json!({}),
                };
                if root.get("auth") != Some(&sanitized) {
                    root.insert("auth".into(), sanitized);
                    changed = true;
                }
            }
        }

        changed
    }

    pub fn has_non_official_provider(&self) -> bool {
        self.apps.iter().any(|(app, data)| {
            data.providers
                .values()
                .any(|provider| !is_official_provider_for_app(app, provider))
        })
    }
}

pub struct StoreSession {
    _lock: config::StoreLock,
    pub root: Root,
    normalized: bool,
    original_contents: Option<Vec<u8>>,
}

impl StoreSession {
    pub fn begin() -> Result<Self, String> {
        let lock = config::lock_store()?;
        let (loaded, original_contents) = read_root_snapshot_unlocked()?;
        let mut root = loaded.unwrap_or_else(Root::default_seeded);
        let normalized = root.ensure_official_providers();
        Ok(Self {
            _lock: lock,
            root,
            normalized,
            original_contents,
        })
    }

    pub fn commit(self) -> Result<Root, String> {
        ensure_store_unchanged(&self.original_contents)?;
        save_unlocked(&self.root)?;
        Ok(self.root)
    }

    pub fn has_changes(&self) -> bool {
        self.normalized
    }

    /// 仅在 begin() 做了结构迁移时落盘，供只读展示路径使用。
    pub fn commit_if_changed(self) -> Result<Root, String> {
        if self.normalized {
            self.commit()
        } else {
            Ok(self.root)
        }
    }
}

/// 严格读取共享配置。缺失文件返回默认结构；损坏文件返回错误，供写路径拒绝覆盖。
pub fn load_checked() -> Result<Root, String> {
    let _lock = config::lock_store()?;
    Ok(read_root_unlocked()?.unwrap_or_else(Root::default_seeded))
}

fn read_root_unlocked() -> Result<Option<Root>, String> {
    read_root_snapshot_unlocked().map(|(root, _)| root)
}

fn read_root_snapshot_unlocked() -> Result<(Option<Root>, Option<Vec<u8>>), String> {
    let path = config::get_store_path();
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => {
            return Err(format!("读取共享配置 {} 失败: {error}", path.display()));
        }
    };
    let root = serde_json::from_slice::<Root>(&contents)
        .map_err(|error| format!("读取共享配置 {} 失败: {error}", path.display()))?;
    Ok((Some(root), Some(contents)))
}

fn ensure_store_unchanged(original_contents: &Option<Vec<u8>>) -> Result<(), String> {
    let path = config::get_store_path();
    let current_contents = match std::fs::read(&path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "提交前重新读取共享配置 {} 失败: {error}",
                path.display()
            ));
        }
    };
    if current_contents.as_ref() != original_contents.as_ref() {
        return Err("共享配置已被 GUI 或另一个进程修改，本次操作已取消；请重新执行命令".into());
    }
    Ok(())
}

fn save_unlocked(root: &Root) -> Result<(), String> {
    config::write_json_file(&config::get_store_path(), root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_claude() {
        let p = Provider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            category: Some("custom".into()),
            settings_config: json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.deepseek.com",
                    "ANTHROPIC_AUTH_TOKEN": "sk-123456",
                    "ANTHROPIC_MODEL": "deepseek-chat"
                }
            }),
            meta: json!({ "apiKeyField": "ANTHROPIC_AUTH_TOKEN" }),
            failover: json!({ "enabled": false }),
        };

        assert_eq!(
            p.extract_base_url("claude").as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(p.extract_api_key("claude").as_deref(), Some("sk-123456"));
        assert_eq!(p.extract_model("claude").as_deref(), Some("deepseek-chat"));
        assert_eq!(
            p.extract_api_key_field("claude").as_deref(),
            Some("ANTHROPIC_AUTH_TOKEN")
        );
    }

    #[test]
    fn claude_key_and_model_extraction_skips_empty_overrides() {
        let p = Provider {
            id: "relay".into(),
            name: "Relay".into(),
            category: Some("custom".into()),
            settings_config: json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://relay.example",
                    "ANTHROPIC_API_KEY": "   ",
                    "ANTHROPIC_AUTH_TOKEN": "token-from-auth",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus"
                }
            }),
            meta: json!({ "apiKeyField": "ANTHROPIC_API_KEY" }),
            failover: json!({}),
        };

        assert_eq!(
            p.extract_api_key("claude").as_deref(),
            Some("token-from-auth")
        );
        assert_eq!(
            p.extract_api_key_field("claude").as_deref(),
            Some("ANTHROPIC_AUTH_TOKEN")
        );
        assert_eq!(p.extract_model("claude").as_deref(), Some("claude-opus"));
    }

    #[test]
    fn test_extract_codex() {
        let p = Provider {
            id: "glm".into(),
            name: "GLM-4".into(),
            category: Some("custom".into()),
            settings_config: json!({
                "auth": { "OPENAI_API_KEY": "glm-token-xyz" },
                "config": "model_provider = \"custom\"\nmodel = \"glm-4-plus\"\n\n[model_providers.custom]\nbase_url = \"https://open.bigmodel.cn/api/paas/v4\"\nwire_api = \"chat\"\n"
            }),
            meta: json!({ "wireApi": "chat" }),
            failover: json!({ "enabled": false }),
        };

        assert_eq!(
            p.extract_base_url("codex").as_deref(),
            Some("https://open.bigmodel.cn/api/paas/v4")
        );
        assert_eq!(p.extract_api_key("codex").as_deref(), Some("glm-token-xyz"));
        assert_eq!(p.extract_model("codex").as_deref(), Some("glm-4-plus"));
        assert_eq!(p.extract_wire_api("codex"), "chat");
    }

    #[test]
    fn test_ensure_official_providers() {
        let mut root = Root {
            version: 1,
            apps: HashMap::new(),
            settings: json!({}),
        };
        let changed = root.ensure_official_providers();
        assert!(changed);
        assert_eq!(root.version, 3);
        assert!(root.apps.contains_key("claude"));
        assert!(root.apps.contains_key("codex"));
        assert!(root.apps.contains_key("grok"));
    }

    #[test]
    fn default_settings_include_grok_import_migration_flag() {
        let root = Root::default_seeded();
        assert_eq!(
            root.settings.get("grokImportDone"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn test_ensure_official_providers_repairs_missing_order_entries() {
        let mut root = Root::default_seeded();
        let data = root.apps.get_mut("claude").unwrap();
        data.providers.insert(
            "relay".into(),
            Provider {
                id: "relay".into(),
                name: "Relay".into(),
                category: Some("custom".into()),
                settings_config: json!({
                    "env": { "ANTHROPIC_BASE_URL": "https://relay.example" }
                }),
                meta: json!({}),
                failover: json!({}),
            },
        );

        assert!(root.ensure_official_providers());
        assert!(root.apps["claude"].order.iter().any(|id| id == "relay"));
    }

    #[test]
    fn ensure_official_providers_preserves_explicitly_untracked_current() {
        let mut root = Root::default_seeded();
        root.apps.get_mut("claude").unwrap().current = None;

        assert!(!root.ensure_official_providers());
        assert!(root.apps["claude"].current.is_none());
    }

    #[test]
    fn official_provider_identity_is_scoped_to_app() {
        let claude_card = official_provider("claude");
        let codex_card = official_provider("codex");

        assert!(is_official_provider_for_app("claude", &claude_card));
        assert!(!is_official_provider_for_app("codex", &claude_card));
        assert!(is_official_provider_for_app("codex", &codex_card));
        assert!(!is_official_provider_for_app("claude", &codex_card));
    }

    #[test]
    fn ensure_official_providers_repairs_provider_id_mismatch() {
        let mut root = Root::default_seeded();
        let data = root.apps.get_mut("grok").unwrap();
        data.providers.insert(
            "actual-id".into(),
            Provider {
                id: "stale-id".into(),
                name: "Relay".into(),
                category: Some("custom".into()),
                settings_config: json!({ "config": "" }),
                meta: json!({}),
                failover: json!({}),
            },
        );

        assert!(root.ensure_official_providers());
        assert_eq!(root.apps["grok"].providers["actual-id"].id, "actual-id");
    }

    #[test]
    fn test_extract_toml_string_handles_nested_tables_and_comments() {
        let config = r#"
model = "model-with-\"quote\"" # inline comment

[model_providers.custom]
base_url = "https://example.test/v1" # inline comment
"#;
        assert_eq!(
            extract_toml_string(config, "model").as_deref(),
            Some("model-with-\"quote\"")
        );
        assert_eq!(
            extract_toml_string(config, "base_url").as_deref(),
            Some("https://example.test/v1")
        );
    }

    #[test]
    fn codex_extraction_follows_selected_model_provider() {
        let config = r#"
model_provider = "selected"

[model_providers.other]
base_url = "https://other.example"
wire_api = "chat"

[model_providers.selected]
base_url = "https://selected.example"
wire_api = "responses"
"#;
        assert_eq!(
            extract_codex_provider_string(config, "base_url").as_deref(),
            Some("https://selected.example")
        );
        assert_eq!(
            extract_codex_provider_string(config, "wire_api").as_deref(),
            Some("responses")
        );
    }

    #[test]
    fn codex_extraction_does_not_use_an_unselected_provider() {
        let config = r#"
model_provider = "selected"

[model_providers.other]
base_url = "https://other.example"
wire_api = "chat"

[model_providers.selected]
wire_api = "responses"
"#;
        assert_eq!(extract_codex_provider_string(config, "base_url"), None);
        assert_eq!(
            extract_codex_provider_string(config, "wire_api").as_deref(),
            Some("responses")
        );
    }

    #[test]
    fn test_quote_toml_string_escapes_input() {
        assert_eq!(
            quote_toml_string("https://example.test/\"quoted\"\\path"),
            r#""https://example.test/\"quoted\"\\path""#
        );
    }

    #[test]
    fn test_grok_config_keeps_gui_compatible_schema_and_key() {
        let config = build_grok_config(
            "Relay (Asia)",
            "https://relay.example/v1",
            "grok-secret",
            "grok-4.5",
            "responses",
        );
        assert!(config.contains("[endpoints]"));
        assert!(config.contains("models_base_url = \"https://relay.example/v1\""));
        assert!(config.contains("api_key = \"grok-secret\""));
        assert!(config.contains("api_backend = \"responses\""));

        let provider = Provider {
            id: "grok".into(),
            name: "Grok".into(),
            category: Some("custom".into()),
            settings_config: json!({ "config": config }),
            meta: json!({}),
            failover: json!({}),
        };
        assert_eq!(
            provider.extract_api_key("grok").as_deref(),
            Some("grok-secret")
        );
        assert_eq!(provider.extract_wire_api("grok"), "responses");
    }

    #[test]
    fn grok_key_extraction_skips_empty_auth_key_before_fallback() {
        let provider = Provider {
            id: "grok-fallback".into(),
            name: "Grok fallback".into(),
            category: Some("custom".into()),
            settings_config: json!({
                "auth": {
                    "GROK_API_KEY": "  ",
                    "XAI_API_KEY": "xai-auth-key"
                },
                "config": "[endpoints]\nmodels_base_url = \"https://relay.example/v1\"\napi_key = \"toml-key\"\n"
            }),
            meta: json!({}),
            failover: json!({}),
        };

        assert_eq!(
            provider.extract_api_key("grok").as_deref(),
            Some("xai-auth-key")
        );

        let toml_fallback = Provider {
            settings_config: json!({
                "auth": { "GROK_API_KEY": "" },
                "config": "[endpoints]\nmodels_base_url = \"https://relay.example/v1\"\napi_key = \"toml-key\"\n"
            }),
            ..provider
        };
        assert_eq!(
            toml_fallback.extract_api_key("grok").as_deref(),
            Some("toml-key")
        );
    }

    #[test]
    fn grok_extraction_follows_default_model() {
        let config = r#"
[models]
default = "selected.model"
web_search = "selected.model"

[endpoints]
models_base_url = "https://relay.example/v1"

[model."other.model"]
model = "other.model"
api_key = "wrong-key"
api_backend = "chat"

[model."selected.model"]
model = "selected.model"
api_key = "selected-key"
api_backend = "responses"
"#;
        assert_eq!(
            extract_grok_default_model(config).as_deref(),
            Some("selected.model")
        );
        assert_eq!(
            extract_grok_model_string(config, "api_key").as_deref(),
            Some("selected-key")
        );
        assert_eq!(
            extract_grok_model_string(config, "api_backend").as_deref(),
            Some("responses")
        );
        let provider = Provider {
            id: "grok".into(),
            name: "Grok".into(),
            category: Some("custom".into()),
            settings_config: json!({ "config": config }),
            meta: json!({}),
            failover: json!({}),
        };
        assert_eq!(
            provider.extract_api_key("grok").as_deref(),
            Some("selected-key")
        );
        assert_eq!(
            provider.extract_model("grok").as_deref(),
            Some("selected.model")
        );
        assert_eq!(provider.extract_wire_api("grok"), "responses");
    }

    #[test]
    fn malformed_grok_config_does_not_read_default_from_another_section() {
        let config = r#"
[other]
default = "wrong-model"

[models]
default = "selected-model"

invalid = [
"#;

        assert_eq!(
            extract_grok_default_model(config).as_deref(),
            Some("selected-model")
        );
    }

    #[test]
    fn malformed_grok_config_does_not_read_default_from_array_table() {
        let config = r#"
[models]
default = "selected-model"

[[other.items]]
default = "wrong-model"

invalid = [
"#;

        assert_eq!(
            extract_grok_default_model(config).as_deref(),
            Some("selected-model")
        );
    }

    #[test]
    fn grok_extraction_falls_back_when_auth_key_is_empty() {
        let config = r#"
[models]
default = "grok-4.5"

[endpoints]
models_base_url = "https://relay.example/v1"

[model."grok-4.5"]
api_key = "toml-key"
"#;
        let provider = Provider {
            id: "relay".into(),
            name: "Relay".into(),
            category: Some("custom".into()),
            settings_config: json!({
                "auth": { "GROK_API_KEY": "   " },
                "config": config
            }),
            meta: json!({}),
            failover: json!({}),
        };

        assert_eq!(
            provider.extract_api_key("grok").as_deref(),
            Some("toml-key")
        );
    }

    #[test]
    fn grok_extraction_does_not_borrow_key_from_another_model() {
        let config = r#"
[models]
default = "selected.model"

[endpoints]
models_base_url = "https://relay.example/v1"

[model."other.model"]
model = "other.model"
api_key = "wrong-key"

[model."selected.model"]
model = "selected.model"
api_backend = "responses"
"#;
        let provider = Provider {
            id: "relay".into(),
            name: "Relay".into(),
            category: Some("custom".into()),
            settings_config: json!({ "config": config }),
            meta: json!({}),
            failover: json!({}),
        };

        assert_eq!(extract_grok_model_string(config, "api_key"), None);
        assert_eq!(provider.extract_api_key("grok"), None);
    }

    #[test]
    fn malformed_grok_config_does_not_read_key_from_array_table() {
        let config = r#"
[models]
default = "selected.model"

[model."selected.model"]
model = "selected.model"

[[model.other]]
api_key = "wrong-key"

invalid = [
"#;

        assert_eq!(extract_grok_model_string(config, "api_key"), None);
    }

    #[test]
    fn malformed_grok_config_does_not_read_endpoint_from_array_table() {
        let config = r#"
[[other.items]]
models_base_url = "https://wrong.example"

invalid = [
"#;

        assert_eq!(
            extract_grok_endpoint_string(config, "models_base_url"),
            None
        );
    }

    #[test]
    fn codex_extraction_does_not_fallback_when_selected_provider_is_incomplete() {
        let config = r#"
model_provider = "selected"

[model_providers.other]
base_url = "https://other.example"
wire_api = "chat"

[model_providers.selected]
wire_api = "responses"
"#;

        assert_eq!(extract_codex_provider_string(config, "base_url"), None);
        assert_eq!(
            extract_codex_provider_string(config, "wire_api").as_deref(),
            Some("responses")
        );
    }

    #[test]
    fn codex_extraction_supports_legacy_root_fields_with_selected_provider() {
        let config = r#"model_provider = "custom"
model = "gpt-4"
base_url = "https://legacy.example/v1"
wire_api = "responses"

[mcp_servers.docs]
base_url = "https://docs.example"
"#;

        assert_eq!(
            extract_codex_provider_string(config, "base_url").as_deref(),
            Some("https://legacy.example/v1")
        );
        assert_eq!(
            extract_codex_provider_string(config, "wire_api").as_deref(),
            Some("responses")
        );
        assert!(!codex_provider_section_exists(config, "custom"));
    }

    #[test]
    fn codex_provider_id_comes_from_the_root_section_only() {
        let config = r#"
model_provider = "selected" # active provider

[model_providers.selected]
base_url = "https://selected.example"

[other]
model_provider = "wrong"
"#;
        assert_eq!(
            extract_codex_provider_id(config).as_deref(),
            Some("selected")
        );
        assert_eq!(
            extract_codex_provider_string(config, "base_url").as_deref(),
            Some("https://selected.example")
        );
    }

    #[test]
    fn malformed_codex_config_does_not_select_provider_from_another_section() {
        let config = r#"
[other]
model_provider = "wrong"

model_provider = "selected"

[model_providers.selected]
base_url = "https://selected.example"
invalid = [
"#;
        assert_eq!(extract_codex_provider_id(config), None);
    }

    #[test]
    fn grok_endpoint_extraction_does_not_borrow_url_from_another_model() {
        let config = r#"
[models]
default = "selected.model"

[model."other.model"]
base_url = "http://127.0.0.1:8999"

[model."selected.model"]
api_key = "selected-key"
"#;

        assert_eq!(
            extract_grok_endpoint_string(config, "models_base_url"),
            None
        );
        assert_eq!(extract_grok_endpoint_string(config, "base_url"), None);
    }

    #[test]
    fn grok_model_extraction_supports_one_legacy_model_without_default() {
        let config = r#"
[model."legacy.model"]
model = "legacy.model"
api_key = "legacy-key"
"#;

        assert_eq!(
            extract_grok_model_string(config, "api_key").as_deref(),
            Some("legacy-key")
        );
        assert_eq!(
            extract_grok_model_string(config, "model").as_deref(),
            Some("legacy.model")
        );
    }

    #[test]
    fn normalization_repairs_grok_order_and_missing_settings() {
        let mut root = Root {
            version: 3,
            apps: HashMap::from([(
                "grok".into(),
                AppData {
                    current: Some("missing".into()),
                    order: vec![],
                    providers: HashMap::from([(
                        "relay".into(),
                        Provider {
                            id: "relay".into(),
                            name: "Relay".into(),
                            category: Some("custom".into()),
                            settings_config: json!({ "config": "" }),
                            meta: json!({}),
                            failover: json!({}),
                        },
                    )]),
                },
            )]),
            settings: Value::Null,
        };

        assert!(root.ensure_official_providers());
        assert_eq!(
            root.settings
                .get("backupBeforeWrite")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(root.apps["grok"].order, vec!["relay"]);
        assert_eq!(root.apps["grok"].current, None);
    }

    #[test]
    fn edit_builders_preserve_advanced_options() {
        let codex = build_codex_config_with_options(
            "Old",
            "https://old.example",
            "old-model",
            "chat",
            "low",
            false,
            true,
            Some(128000),
        );
        let edited = build_codex_config_preserving(
            &codex,
            "New",
            "https://new.example",
            "new-model",
            "responses",
        );
        assert!(edited.contains("model_reasoning_effort = \"low\""));
        assert!(edited.contains("disable_response_storage = false"));
        assert!(edited.contains("requires_openai_auth = true"));
        assert!(edited.contains("model_context_window = 128000"));
        assert!(edited.contains("base_url = \"https://new.example\""));

        let grok = build_grok_config_with_context(
            "Old",
            "https://old.example",
            "old-key",
            "old-model",
            "chat",
            64000,
        );
        let edited_grok = build_grok_config_preserving(
            &grok,
            "New",
            "https://new.example",
            "new-key",
            "new-model",
            "responses",
        );
        assert!(edited_grok.contains("context_window = 64000"));
        assert!(edited_grok.contains("api_key = \"new-key\""));
    }

    #[test]
    fn edit_builders_preserve_unmanaged_toml_sections() {
        let codex = r#"
model_provider = "custom"
model = "old-model"

[mcp_servers.docs]
command = "docs-mcp"

[model_providers.custom]
name = "Old"
base_url = "https://old.example"
wire_api = "chat"
requires_openai_auth = true
"#;
        let edited = build_codex_config_preserving(
            codex,
            "New",
            "https://new.example",
            "new-model",
            "responses",
        );
        assert!(edited.contains("[mcp_servers.docs]"));
        assert!(edited.contains("command = \"docs-mcp\""));
        assert!(edited.contains("base_url = \"https://new.example\""));
        assert!(edited.contains("wire_api = \"responses\""));
        assert!(edited.contains("requires_openai_auth = true"));

        let grok = r#"
[models]
default = "old-model"
web_search = "old-model"

[endpoints]
models_base_url = "https://old.example"

[model.old-model]
model = "old-model"
name = "Old"
description = "Old description"
api_key = "old-key"
api_backend = "chat"
context_window = 64000

[model.other-model]
model = "other-model"
name = "Other"
api_key = "other-key"

[custom]
keep_me = true
"#;
        let edited_grok = build_grok_config_preserving(
            grok,
            "New",
            "https://new.example",
            "new-key",
            "new-model",
            "responses",
        );
        assert!(edited_grok.contains("[model.other-model]"));
        assert!(edited_grok.contains("api_key = \"other-key\""));
        assert!(edited_grok.contains("[custom]"));
        assert!(edited_grok.contains("keep_me = true"));
        assert!(edited_grok.contains("context_window = 64000"));
        assert!(edited_grok.contains("api_key = \"new-key\""));
        assert!(!edited_grok.contains("[model.old-model]"));
    }

    #[test]
    fn grok_editing_to_existing_model_does_not_drop_old_model() {
        let grok = r#"
[models]
default = "old-model"
web_search = "old-model"

[endpoints]
models_base_url = "https://old.example"

[model.old-model]
model = "old-model"
name = "Old"
api_key = "old-key"
api_backend = "chat"

[model.other-model]
model = "other-model"
name = "Other"
api_key = "other-key"
api_backend = "responses"
context_window = 128000
"#;

        let edited = build_grok_config_preserving(
            grok,
            "Renamed other",
            "https://new.example",
            "new-key",
            "other-model",
            "chat",
        );

        assert!(edited.contains("[model.old-model]"));
        assert!(edited.contains("api_key = \"old-key\""));
        assert!(edited.contains("[model.other-model]"));
        assert!(edited.contains("name = \"Renamed other\""));
        assert!(edited.contains("api_key = \"new-key\""));
        assert!(edited.contains("context_window = 128000"));
        assert!(!edited.contains("default = \"old-model\""));
    }
}
