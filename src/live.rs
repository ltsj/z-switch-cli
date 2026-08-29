//! 阶段 2：切换时写 live 配置。
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

use crate::config;
use crate::store::{self, Provider};

const CLAUDE_RELAY_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
];

fn read_obj(path: &Path) -> Result<Map<String, Value>, String> {
    match fs::read_to_string(path) {
        Ok(s) => {
            if s.trim().is_empty() {
                return Ok(Map::new());
            }
            let v: Value = serde_json::from_str(&s).map_err(|e| {
                format!(
                    "现有文件 {} 不是合法 JSON，已中止写入以防丢失你的其它配置：{e}",
                    path.display()
                )
            })?;
            match v {
                Value::Object(m) => Ok(m),
                _ => Err(format!(
                    "现有文件 {} 不是 JSON 对象，已中止写入以防覆盖",
                    path.display()
                )),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("读取 {} 失败: {e}", path.display())),
    }
}

pub fn backup_file(path: &Path, tag: &str) {
    if !path.exists() {
        return;
    }
    let dir = config::get_app_config_dir().join("backups");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dest = dir.join(format!("{tag}-{ts}.bak"));
    let _ = fs::copy(path, dest);
    prune_old_backups(&dir, 60);
}

fn prune_old_backups(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                if let Ok(time) = meta.modified() {
                    files.push((entry.path(), time));
                }
            }
        }
    }
    if files.len() > keep {
        files.sort_by_key(|(_, time)| *time);
        let remove_count = files.len() - keep;
        for (path, _) in files.into_iter().take(remove_count) {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn backup_current_app(app: &str) {
    match app {
        "claude" => backup_file(&config::get_claude_settings_path(), "claude-settings"),
        "codex" => {
            backup_file(&config::get_codex_auth_path(), "codex-auth");
            backup_file(&config::get_codex_config_path(), "codex-config");
        }
        "grok" => backup_file(&config::get_grok_config_path(), "grok-config"),
        _ => {}
    }
}

// ---------- Claude ----------

fn read_claude_live_env() -> Option<Value> {
    let path = config::get_claude_settings_path();
    read_obj(&path).ok().and_then(|o| o.get("env").cloned())
}

fn sanitize_claude_official_env(env: &Value) -> Value {
    let mut object = env.as_object().cloned().unwrap_or_default();
    for key in CLAUDE_RELAY_ENV_KEYS {
        object.remove(*key);
    }
    Value::Object(object)
}

fn write_claude_live(env: &Value, backup: bool) -> Result<(), String> {
    let path = config::get_claude_settings_path();
    let mut settings = read_obj(&path)?;
    if backup {
        backup_file(&path, "claude-settings");
    }
    settings.insert("env".into(), env.clone());
    config::write_json_file(&path, &Value::Object(settings))
}

pub fn write_official_baseline(app: &str, backup: bool) -> Result<(), String> {
    match app {
        "claude" => {
            let path = config::get_claude_settings_path();
            if backup {
                backup_file(&path, "claude-settings");
            }
            match read_obj(&path) {
                Ok(mut settings) => {
                    let env = settings
                        .get("env")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    settings.insert("env".into(), sanitize_claude_official_env(&env));
                    config::write_json_file(&path, &Value::Object(settings))
                }
                Err(_) => {
                    config::write_json_file(&path, &serde_json::json!({ "env": {} }))
                }
            }
        }
        "codex" => {
            let auth = crate::official::codex_auth_for_restore()?;
            write_codex_live(&auth, "", backup)
        }
        other => Err(format!("未知应用: {other}")),
    }
}

// ---------- Codex ----------

fn read_codex_live() -> (Option<Value>, Option<String>) {
    let auth_path = config::get_codex_auth_path();
    let cfg_path = config::get_codex_config_path();
    let auth = fs::read_to_string(&auth_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let cfg = fs::read_to_string(&cfg_path).ok();
    (auth, cfg)
}

fn write_codex_live(auth: &Value, config_text: &str, backup: bool) -> Result<(), String> {
    let auth_path = config::get_codex_auth_path();
    let cfg_path = config::get_codex_config_path();
    if backup {
        backup_file(&auth_path, "codex-auth");
        backup_file(&cfg_path, "codex-config");
    }

    let old_auth = fs::read(&auth_path).ok();
    config::write_json_file(&auth_path, auth)?;

    if let Err(e) = config::write_text_file(&cfg_path, config_text) {
        match old_auth {
            Some(bytes) => {
                let _ = config::atomic_write(&auth_path, &bytes);
            }
            None => {
                let _ = fs::remove_file(&auth_path);
            }
        }
        return Err(format!("写入 config.toml 失败，已回滚 auth.json：{e}"));
    }
    Ok(())
}

fn sanitize_codex_official_config(config_text: &str) -> String {
    fn model_provider_value(line: &str) -> Option<&str> {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            return None;
        }
        let (key, value) = trimmed.split_once('=')?;
        (key.trim() == "model_provider").then_some(value.trim())
    }

    let provider_id = config_text
        .lines()
        .find_map(model_provider_value)
        .map(|value| value.trim_matches(['\"', '\'']).to_string())
        .filter(|value| !value.is_empty() && value != "openai");

    let mut result = Vec::new();
    let mut skip_provider_table = false;
    for line in config_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = trimmed[1..trimmed.len() - 1].replace(['\"', '\''], "");
            skip_provider_table = provider_id.as_ref().is_some_and(|id| {
                let provider_section = format!("model_providers.{id}");
                section == provider_section || section.starts_with(&(provider_section + "."))
            });
            if skip_provider_table {
                continue;
            }
        }
        if skip_provider_table {
            continue;
        }
        if model_provider_value(trimmed).is_some() {
            continue;
        }
        result.push(line);
    }

    while result.last().is_some_and(|line| line.trim().is_empty()) {
        result.pop();
    }
    if result.is_empty() {
        String::new()
    } else {
        result.join("\n") + "\n"
    }
}

pub fn hydrate_official_provider(app: &str, provider: &mut Provider) -> bool {
    if !store::is_official_provider(provider) {
        return false;
    }
    match app {
        "claude" => {
            let current = provider
                .settings_config
                .get("env")
                .and_then(Value::as_object);
            if current.is_some_and(|env| !env.is_empty()) {
                return false;
            }
            let Some(live) = read_claude_live_env() else {
                return false;
            };
            let sanitized = sanitize_claude_official_env(&live);
            if sanitized.as_object().is_none_or(|env| env.is_empty()) {
                return false;
            }
            provider.settings_config = serde_json::json!({ "env": sanitized });
            true
        }
        "codex" => {
            let current = provider
                .settings_config
                .get("config")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !current.trim().is_empty() {
                return false;
            }
            let (_, live_config) = read_codex_live();
            let sanitized = sanitize_codex_official_config(live_config.as_deref().unwrap_or(""));
            if sanitized.trim().is_empty() {
                return false;
            }
            provider.settings_config = serde_json::json!({ "auth": {}, "config": sanitized });
            true
        }
        _ => false,
    }
}

// ---------- Grok ----------

fn read_grok_live() -> Option<String> {
    fs::read_to_string(config::get_grok_config_path()).ok()
}

fn write_grok_live(config_text: &str, backup: bool) -> Result<(), String> {
    let path = config::get_grok_config_path();
    if backup {
        backup_file(&path, "grok-config");
    }
    config::write_text_file(&path, config_text)
}

// ---------- 对外统一入口 ----------

pub fn write_live(app: &str, provider: &Provider, backup: bool) -> Result<(), String> {
    let official = store::is_official_provider(provider);
    match app {
        "claude" => {
            let mut env = provider
                .settings_config
                .get("env")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            if official {
                env = sanitize_claude_official_env(&env);
            }
            write_claude_live(&env, backup)
        }
        "codex" => {
            let auth = if official {
                crate::official::codex_auth_for_restore()?
            } else {
                provider
                    .settings_config
                    .get("auth")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Map::new()))
            };
            let mut cfg = provider
                .settings_config
                .get("config")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if official {
                cfg = sanitize_codex_official_config(&cfg);
            }
            write_codex_live(&auth, &cfg, backup)
        }
        "grok" => {
            let cfg = provider
                .settings_config
                .get("config")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            write_grok_live(cfg, backup)
        }
        other => Err(format!("未知应用: {other}")),
    }
}

pub fn backfill(app: &str, provider: &mut Provider) {
    let official = store::is_official_provider(provider);
    let obj = match provider.settings_config.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    match app {
        "claude" => {
            if let Some(env) = read_claude_live_env() {
                if !official {
                    if let Some(base) = env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str) {
                        if crate::repair::is_localhost(base) {
                            return;
                        }
                    }
                    if let Some(key) = env.get("ANTHROPIC_AUTH_TOKEN").or_else(|| env.get("ANTHROPIC_API_KEY")).and_then(Value::as_str) {
                        if key == crate::proxy::PLACEHOLDER_KEY {
                            return;
                        }
                    }
                }
                obj.insert(
                    "env".into(),
                    if official {
                        sanitize_claude_official_env(&env)
                    } else {
                        env
                    },
                );
            }
        }
        "codex" => {
            let (auth, cfg) = read_codex_live();
            if official {
                if let Err(error) = crate::official::capture_codex_current() {
                    eprintln!("[z-switch] 保存 Codex 官方登录态失败：{error}");
                }
                obj.insert("auth".into(), serde_json::json!({}));
            } else {
                if let Some(auth_val) = &auth {
                    if let Some(key) = auth_val.get("OPENAI_API_KEY").and_then(Value::as_str) {
                        if key == crate::proxy::PLACEHOLDER_KEY {
                            return;
                        }
                    }
                }
                if let Some(cfg_val) = &cfg {
                    if cfg_val.lines().any(|l| {
                        let (k, v) = l.trim().split_once('=').unwrap_or(("", ""));
                        k.trim() == "base_url" && crate::repair::is_localhost(v.trim().trim_matches(['\"', '\'']))
                    }) {
                        return;
                    }
                }
                if let Some(auth) = auth {
                    let key = auth
                        .get("OPENAI_API_KEY")
                        .cloned()
                        .unwrap_or(Value::String(String::new()));
                    obj.insert("auth".into(), serde_json::json!({ "OPENAI_API_KEY": key }));
                }
            }
            if let Some(cfg) = cfg {
                obj.insert(
                    "config".into(),
                    Value::String(if official {
                        sanitize_codex_official_config(&cfg)
                    } else {
                        cfg
                    }),
                );
            }
        }
        "grok" => {
            if let Some(cfg) = read_grok_live() {
                if cfg.lines().any(|l| {
                    let (k, v) = l.trim().split_once('=').unwrap_or(("", ""));
                    (k.trim() == "base_url" || k.trim() == "models_base_url")
                        && crate::repair::is_localhost(v.trim().trim_matches(['\"', '\'']))
                }) {
                    return;
                }
                obj.insert("config".into(), Value::String(cfg));
            }
        }
        _ => {}
    }
}

fn host_of(url: &str) -> Option<String> {
    let s = url.split("://").last()?;
    let host = s.split('/').next()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

pub fn import_claude() -> Option<Provider> {
    let env = read_claude_live_env()?;
    let env_obj = env.as_object()?;
    let base = env_obj.get("ANTHROPIC_BASE_URL").and_then(|v| v.as_str());
    let base = base?;
    if base.trim().is_empty() || crate::repair::is_localhost(base) {
        return None;
    }
    let key_field = if env_obj.contains_key("ANTHROPIC_API_KEY") {
        "ANTHROPIC_API_KEY"
    } else {
        "ANTHROPIC_AUTH_TOKEN"
    };
    if let Some(k) = env_obj.get(key_field).and_then(|v| v.as_str()) {
        if k == crate::proxy::PLACEHOLDER_KEY {
            return None;
        }
    }
    let name = host_of(base).unwrap_or_else(|| "导入的 Claude 供应商".to_string());
    Some(Provider {
        id: "imported-current".to_string(),
        name,
        category: Some("imported".into()),
        settings_config: serde_json::json!({ "env": env.clone() }),
        meta: serde_json::json!({ "apiKeyField": key_field, "imported": true }),
        failover: serde_json::json!({ "enabled": false }),
    })
}

pub fn import_codex() -> Option<Provider> {
    let (auth, cfg) = read_codex_live();
    let cfg = cfg?;
    if cfg.trim().is_empty() {
        return None;
    }
    let base = cfg.lines().find_map(|l| {
        let (k, v) = l.trim().split_once('=')?;
        if k.trim() == "base_url" {
            Some(v.trim().trim_matches(['\"', '\'']).to_string())
        } else {
            None
        }
    });
    let base = base.filter(|value| !value.trim().is_empty() && !crate::repair::is_localhost(value))?;
    if let Some(auth_val) = &auth {
        if let Some(key) = auth_val.get("OPENAI_API_KEY").and_then(Value::as_str) {
            if key == crate::proxy::PLACEHOLDER_KEY {
                return None;
            }
        }
    }
    let wire = cfg
        .lines()
        .find_map(|l| {
            let (k, v) = l.trim().split_once('=')?;
            if k.trim() == "wire_api" {
                Some(v.trim().trim_matches(['\"', '\'']).to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "responses".to_string());
    let name = host_of(&base).unwrap_or_else(|| "导入的 Codex 供应商".to_string());
    let key = auth
        .as_ref()
        .and_then(|value| value.get("OPENAI_API_KEY"))
        .cloned()
        .unwrap_or(Value::String(String::new()));
    Some(Provider {
        id: "imported-current".to_string(),
        name,
        category: Some("imported".into()),
        settings_config: serde_json::json!({
            "auth": { "OPENAI_API_KEY": key },
            "config": cfg
        }),
        meta: serde_json::json!({ "wireApi": wire, "imported": true }),
        failover: serde_json::json!({ "enabled": false }),
    })
}

pub fn import_grok() -> Option<Provider> {
    let cfg = read_grok_live()?;
    if cfg.trim().is_empty() {
        return None;
    }
    let base = cfg.lines().find_map(|l| {
        let (k, v) = l.trim().split_once('=')?;
        if k.trim() == "models_base_url" || k.trim() == "base_url" {
            Some(v.trim().trim_matches(['\"', '\'']).to_string())
        } else {
            None
        }
    });
    let base = base.filter(|value| !value.trim().is_empty() && !crate::repair::is_localhost(value))?;
    let name = host_of(&base).unwrap_or_else(|| "导入的 Grok 供应商".to_string());
    Some(Provider {
        id: "imported-current".to_string(),
        name,
        category: Some("imported".into()),
        settings_config: serde_json::json!({ "config": cfg }),
        meta: serde_json::json!({ "imported": true }),
        failover: serde_json::json!({ "enabled": false }),
    })
}
