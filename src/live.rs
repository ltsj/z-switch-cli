//! 阶段 2：切换时写 live 配置。
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

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
                return Err(format!(
                    "现有文件 {} 为空，已中止写入以防覆盖你的其它配置",
                    path.display()
                ));
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
            // The directory is user-visible and may contain manual notes or
            // other backups. Match the exact names emitted by `backup_file`;
            // a generic `.bak` suffix is not ownership proof.
            let is_z_switch_backup = entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.rsplit_once('-'))
                .is_some_and(|(tag, timestamp)| {
                    matches!(
                        tag,
                        "claude-settings" | "codex-auth" | "codex-config" | "grok-config"
                    ) && !timestamp.is_empty()
                        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
                        && entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "bak")
                });
            if meta.is_file() && is_z_switch_backup {
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

#[derive(Clone)]
struct LiveFileSnapshot {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

/// 写入 live 配置前保存当前应用涉及的全部文件，用于 providers.json 提交
/// 失败或后续 IPC 失败时恢复到操作前状态。
#[derive(Clone)]
pub struct AppLiveSnapshot {
    files: Vec<LiveFileSnapshot>,
}

fn app_paths(app: &str) -> Result<Vec<PathBuf>, String> {
    match app {
        "claude" => Ok(vec![config::get_claude_settings_path()]),
        "codex" => Ok(vec![
            config::get_codex_auth_path(),
            config::get_codex_config_path(),
        ]),
        "grok" => Ok(vec![config::get_grok_config_path()]),
        other => Err(format!("未知应用: {other}")),
    }
}

pub fn snapshot_app(app: &str) -> Result<AppLiveSnapshot, String> {
    let files = app_paths(app)?
        .into_iter()
        .map(|path| match fs::read(&path) {
            Ok(content) => Ok(LiveFileSnapshot {
                path,
                content: Some(content),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LiveFileSnapshot {
                path,
                content: None,
            }),
            Err(error) => Err(format!("读取 {} 失败: {error}", path.display())),
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(AppLiveSnapshot { files })
}

pub fn restore_snapshot(snapshot: &AppLiveSnapshot) -> Result<(), String> {
    let mut applied = Vec::new();
    for file in &snapshot.files {
        let previous = match fs::read(&file.path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                let detail = format!("读取 {} 当前状态失败: {error}", file.path.display());
                return Err(with_restore_rollback_error(detail, &applied));
            }
        };

        let result = match &file.content {
            Some(content) => config::atomic_write(&file.path, content),
            None => match fs::remove_file(&file.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("删除 {} 失败: {error}", file.path.display())),
            },
        };
        if let Err(error) = result {
            return Err(with_restore_rollback_error(error, &applied));
        }
        applied.push((file.path.clone(), previous));
    }
    Ok(())
}

fn with_restore_rollback_error(
    original_error: String,
    applied: &[(PathBuf, Option<Vec<u8>>)],
) -> String {
    let mut rollback_errors = Vec::new();
    for (path, content) in applied.iter().rev() {
        let result = match content {
            Some(bytes) => config::atomic_write(path, bytes),
            None => match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("删除 {} 失败: {error}", path.display())),
            },
        };
        if let Err(error) = result {
            rollback_errors.push(format!("恢复 {} 失败: {error}", path.display()));
        }
    }
    if rollback_errors.is_empty() {
        format!("{original_error}；已回滚已恢复的文件")
    } else {
        format!(
            "{original_error}；文件恢复回滚失败: {}",
            rollback_errors.join("；")
        )
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
            // 官方恢复是显式的自愈操作：坏掉的 settings.json 已经备份，
            // 此处应重建最小合法对象，而不是再次被损坏文件阻塞。
            let mut settings = read_obj(&path).unwrap_or_default();
            let env = settings
                .get("env")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            settings.insert("env".into(), sanitize_claude_official_env(&env));
            config::write_json_file(&path, &Value::Object(settings))
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
    let before = snapshot_app("codex")?;
    if backup {
        backup_file(&auth_path, "codex-auth");
        backup_file(&cfg_path, "codex-config");
    }

    let result = config::write_json_file(&auth_path, auth)
        .and_then(|()| config::write_text_file(&cfg_path, config_text));
    if let Err(error) = result {
        return match restore_snapshot(&before) {
            Ok(()) => Err(format!("写入 Codex 配置失败，已回滚本次部分修改：{error}")),
            Err(rollback_error) => Err(format!(
                "写入 Codex 配置失败，回滚也失败: {error}; {rollback_error}"
            )),
        };
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

    fn model_provider_id(line: &str) -> Option<String> {
        let value = model_provider_value(line)?;
        crate::store::parse_toml_string_value(value)
            .or_else(|| Some(value.trim_matches(['\"', '\'']).to_string()))
            .filter(|value| !value.trim().is_empty())
    }

    let provider_id = crate::store::extract_codex_provider_id(config_text)
        .or_else(|| config_text.lines().find_map(model_provider_id))
        .filter(|value| !value.is_empty() && value != "openai");

    let mut result = Vec::new();
    let mut skip_provider_table = false;
    let mut current_section = None;
    let mut in_array_table = false;
    for line in config_text.lines() {
        let trimmed = line.trim();
        if crate::store::is_toml_array_section(trimmed) {
            // An array-of-tables starts a new context. Do not carry the
            // selected provider's skip state into unrelated array entries.
            current_section = None;
            in_array_table = true;
            skip_provider_table = false;
            result.push(line);
            continue;
        }
        if let Some(section) = crate::store::normalized_toml_section(trimmed) {
            current_section = Some(section.clone());
            in_array_table = false;
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
        if !in_array_table && current_section.is_none() && model_provider_value(trimmed).is_some() {
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
    if !store::is_official_provider_for_app(app, provider) {
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
    let official = store::is_official_provider_for_app(app, provider);
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
    let official = store::is_official_provider_for_app(app, provider);
    let obj = match provider.settings_config.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    match app {
        "claude" => {
            if let Some(env) = read_claude_live_env() {
                if !official {
                    if proxy_port("claude").is_some() {
                        return;
                    }
                    if ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
                        .iter()
                        .filter_map(|field| env.get(*field).and_then(Value::as_str))
                        .any(crate::proxy::is_placeholder_key)
                    {
                        return;
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
                let auth_is_placeholder = auth
                    .as_ref()
                    .and_then(|value| value.get("OPENAI_API_KEY"))
                    .and_then(Value::as_str)
                    .is_some_and(crate::proxy::is_placeholder_key);
                if !auth_is_placeholder && proxy_port("codex").is_none() {
                    if let Err(error) = crate::official::capture_codex_current() {
                        eprintln!("[z-switch] 保存 Codex 官方登录态失败：{error}");
                    }
                }
                obj.insert("auth".into(), serde_json::json!({}));
            } else {
                if let Some(auth_val) = &auth {
                    if let Some(key) = auth_val.get("OPENAI_API_KEY").and_then(Value::as_str) {
                        if crate::proxy::is_placeholder_key(key) {
                            return;
                        }
                    }
                }
                if proxy_port("codex").is_some() {
                    return;
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
                if proxy_port("grok").is_some() {
                    return;
                }
                if ["api_key", "grok_api_key", "xai_api_key"]
                    .iter()
                    .filter_map(|key| {
                        crate::store::extract_grok_endpoint_string(&cfg, key)
                            .or_else(|| crate::store::extract_grok_model_string(&cfg, key))
                    })
                    .any(|key| crate::proxy::is_placeholder_key(&key))
                {
                    return;
                }
                obj.insert("config".into(), Value::String(cfg));
            }
        }
        _ => {}
    }
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url.trim())
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
}

fn claude_key_field(env: &Map<String, Value>) -> &'static str {
    if env
        .get("ANTHROPIC_API_KEY")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        "ANTHROPIC_API_KEY"
    } else {
        "ANTHROPIC_AUTH_TOKEN"
    }
}

/// 读取指定应用当前 live 配置指向的本地代理端口。
/// 用于后续 edit/remove/repair 操作接管曾由自定义端口启动的代理。
pub fn proxy_port(app: &str) -> Option<u16> {
    let base = match app {
        "claude" => read_claude_live_env()?
            .get("ANTHROPIC_BASE_URL")
            .and_then(Value::as_str)
            .map(str::to_string)?,
        "codex" => read_codex_live()
            .1
            .as_deref()
            .and_then(|cfg| crate::store::extract_codex_provider_string(cfg, "base_url"))?,
        "grok" => {
            let cfg = read_grok_live()?;
            crate::store::extract_grok_endpoint_string(&cfg, "models_base_url")
                .or_else(|| crate::store::extract_grok_endpoint_string(&cfg, "base_url"))?
        }
        _ => return None,
    };
    proxy_port_from_base(app, &base)
}

/// 从操作前的 live 快照识别 z-switch 自己写入的代理端口。
///
/// 回滚时不能直接把快照写回：如果代理在操作期间已经退出，写回
/// `127.0.0.1:<port>/<app>` 会让客户端继续指向失效地址。这里读取快照
/// 本身，而不是当前磁盘文件，避免回滚判断被中间写入的内容污染。
pub fn snapshot_proxy_port(snapshot: &AppLiveSnapshot, app: &str) -> Option<u16> {
    let base = match app {
        "claude" => snapshot.files.iter().find_map(|file| {
            (file.path == config::get_claude_settings_path()).then(|| {
                let value = serde_json::from_slice::<Value>(file.content.as_ref()?).ok()?;
                value
                    .get("env")
                    .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })?
        }),
        "codex" => snapshot.files.iter().find_map(|file| {
            (file.path == config::get_codex_config_path()).then(|| {
                let content = String::from_utf8(file.content.as_ref()?.clone()).ok()?;
                crate::store::extract_codex_provider_string(&content, "base_url")
            })?
        }),
        "grok" => snapshot.files.iter().find_map(|file| {
            (file.path == config::get_grok_config_path()).then(|| {
                let content = String::from_utf8(file.content.as_ref()?.clone()).ok()?;
                crate::store::extract_grok_endpoint_string(&content, "models_base_url")
                    .or_else(|| crate::store::extract_grok_endpoint_string(&content, "base_url"))
            })?
        }),
        _ => None,
    }?;
    proxy_port_from_base(app, &base)
}

fn proxy_port_from_base(app: &str, base: &str) -> Option<u16> {
    let url = reqwest::Url::parse(base).ok()?;
    if !crate::repair::is_localhost(url.as_str()) {
        return None;
    }

    // z-switch 写入的 live 代理地址固定为
    // `http://127.0.0.1:<port>/<app>`。仅凭 localhost 不能区分本地
    // Ollama/LM Studio 等真实上游，否则直连本地模型会被误判为外部代理。
    let expected_path = format!("/{app}");
    (url.path().trim_end_matches('/') == expected_path).then(|| url.port_or_known_default())?
}

pub fn import_claude() -> Option<Provider> {
    let env = read_claude_live_env()?;
    let env_obj = env.as_object()?;
    let base = env_obj.get("ANTHROPIC_BASE_URL").and_then(|v| v.as_str());
    let base = base?;
    if base.trim().is_empty()
        || proxy_port("claude").is_some()
        || crate::proxy::validate_base_url(base).is_err()
    {
        return None;
    }
    let key_field = claude_key_field(env_obj);
    if ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
        .iter()
        .filter_map(|field| env_obj.get(*field).and_then(Value::as_str))
        .any(crate::proxy::is_placeholder_key)
    {
        return None;
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
    let base = crate::store::extract_codex_provider_string(&cfg, "base_url");
    let base = base.filter(|value| {
        !value.trim().is_empty()
            && proxy_port("codex").is_none()
            && crate::proxy::validate_base_url(value).is_ok()
    })?;
    if let Some(auth_val) = &auth {
        if let Some(key) = auth_val.get("OPENAI_API_KEY").and_then(Value::as_str) {
            if crate::proxy::is_placeholder_key(key) {
                return None;
            }
        }
    }
    let wire = crate::store::extract_codex_provider_string(&cfg, "wire_api")
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
    let base = crate::store::extract_grok_endpoint_string(&cfg, "models_base_url")
        .or_else(|| crate::store::extract_grok_endpoint_string(&cfg, "base_url"));
    let base = base.filter(|value| {
        !value.trim().is_empty()
            && proxy_port("grok").is_none()
            && crate::proxy::validate_base_url(value).is_ok()
    })?;
    if ["api_key", "grok_api_key", "xai_api_key"]
        .iter()
        .filter_map(|key| {
            crate::store::extract_grok_endpoint_string(&cfg, key)
                .or_else(|| crate::store::extract_grok_model_string(&cfg, key))
        })
        .any(|key| crate::proxy::is_placeholder_key(&key))
    {
        return None;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_port_only_matches_z_switch_app_path() {
        assert_eq!(
            proxy_port_from_base("claude", "http://127.0.0.1:8999/claude"),
            Some(8999)
        );
        assert_eq!(
            proxy_port_from_base("claude", "http://127.0.0.1:8999/claude/"),
            Some(8999)
        );
        assert_eq!(
            proxy_port_from_base("claude", "http://127.0.0.1:11434/v1"),
            None
        );
        assert_eq!(
            proxy_port_from_base("claude", "https://api.example.com/claude"),
            None
        );
    }

    #[test]
    fn snapshot_proxy_port_reads_cli_routes_without_touching_disk() {
        let claude_snapshot = AppLiveSnapshot {
            files: vec![LiveFileSnapshot {
                path: config::get_claude_settings_path(),
                content: Some(
                    br#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:9123/claude"}}"#.to_vec(),
                ),
            }],
        };
        assert_eq!(snapshot_proxy_port(&claude_snapshot, "claude"), Some(9123));

        let codex_snapshot = AppLiveSnapshot {
            files: vec![LiveFileSnapshot {
                path: config::get_codex_config_path(),
                content: Some(
                    b"model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"http://localhost:9234/codex\"\n"
                        .to_vec(),
                ),
            }],
        };
        assert_eq!(snapshot_proxy_port(&codex_snapshot, "codex"), Some(9234));

        let grok_snapshot = AppLiveSnapshot {
            files: vec![LiveFileSnapshot {
                path: config::get_grok_config_path(),
                content: Some(
                    b"[endpoints]\nmodels_base_url = \"http://127.0.0.1:9345/grok\"\n".to_vec(),
                ),
            }],
        };
        assert_eq!(snapshot_proxy_port(&grok_snapshot, "grok"), Some(9345));
    }

    #[test]
    fn snapshot_proxy_port_rejects_non_cli_localhost_urls() {
        let snapshot = AppLiveSnapshot {
            files: vec![LiveFileSnapshot {
                path: config::get_claude_settings_path(),
                content: Some(
                    br#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:9123/v1"}}"#.to_vec(),
                ),
            }],
        };
        assert_eq!(snapshot_proxy_port(&snapshot, "claude"), None);
    }

    #[test]
    fn official_codex_config_removes_commented_provider_table() {
        let input = r#"model_provider = "custom" # selected relay
model = "gpt-5"

[model_providers.custom] # relay
name = "Relay"
base_url = "https://relay.example/v1"
wire_api = "responses"

[mcp_servers.docs]
command = "docs"
"#;

        let output = sanitize_codex_official_config(input);
        assert!(!output.contains("model_provider ="));
        assert!(!output.contains("relay.example"));
        assert!(output.contains("model = \"gpt-5\""));
        assert!(output.contains("[mcp_servers.docs]"));
    }

    #[test]
    fn official_codex_sanitizing_preserves_unrelated_array_tables() {
        let input = r#"model_provider = "custom"

[model_providers.custom]
base_url = "https://relay.example/v1"

[[mcp_servers.docs]]
model_provider = "docs-provider"
base_url = "https://docs.example"
"#;

        let output = sanitize_codex_official_config(input);
        assert!(!output.contains("model_provider = \"custom\""));
        assert!(!output.contains("relay.example"));
        assert!(output.contains("[[mcp_servers.docs]]"));
        assert!(output.contains("model_provider = \"docs-provider\""));
        assert!(output.contains("base_url = \"https://docs.example\""));
    }

    #[test]
    fn pruning_backups_keeps_non_backup_files() {
        let dir = std::env::temp_dir().join(format!(
            "z_switch_backup_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("claude-settings-1.bak"), b"1").unwrap();
        fs::write(dir.join("claude-settings-2.bak"), b"2").unwrap();
        fs::write(dir.join("manual.bak"), b"keep").unwrap();
        fs::write(dir.join("manual.txt"), b"keep").unwrap();

        prune_old_backups(&dir, 1);

        let backups = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with("claude-settings-"))
            })
            .count();
        assert_eq!(backups, 1);
        assert_eq!(fs::read(dir.join("manual.bak")).unwrap(), b"keep");
        assert_eq!(fs::read(dir.join("manual.txt")).unwrap(), b"keep");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn restore_snapshot_rolls_back_previous_files_after_later_failure() {
        let dir = std::env::temp_dir().join(format!(
            "z_switch_restore_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.json");
        fs::write(&first, b"before").unwrap();
        let second = dir.join("second.toml");
        fs::create_dir(&second).unwrap();

        let snapshot = AppLiveSnapshot {
            files: vec![
                LiveFileSnapshot {
                    path: first.clone(),
                    content: Some(b"after".to_vec()),
                },
                LiveFileSnapshot {
                    path: second,
                    content: None,
                },
            ],
        };

        assert!(restore_snapshot(&snapshot).is_err());
        assert_eq!(fs::read(&first).unwrap(), b"before");
        let _ = fs::remove_dir_all(dir);
    }
}
