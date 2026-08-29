//! providers.json 数据模型 + 读写。
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::config;

pub const CLAUDE_OFFICIAL_PROVIDER_ID: &str = "claude-official";
pub const CODEX_OFFICIAL_PROVIDER_ID: &str = "codex-official";

pub fn official_provider_id(app: &str) -> Option<&'static str> {
    match app {
        "claude" => Some(CLAUDE_OFFICIAL_PROVIDER_ID),
        "codex" => Some(CODEX_OFFICIAL_PROVIDER_ID),
        _ => None,
    }
}

pub fn is_official_provider(provider: &Provider) -> bool {
    matches!(
        provider.id.as_str(),
        CLAUDE_OFFICIAL_PROVIDER_ID | CODEX_OFFICIAL_PROVIDER_ID
    )
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
                .map(|s| s.to_string()),
            "codex" => {
                let cfg = self.settings_config.get("config").and_then(|v| v.as_str())?;
                cfg.lines().find_map(|l| {
                    let trimmed = l.trim();
                    let (k, v) = trimmed.split_once('=')?;
                    if k.trim() == "base_url" {
                        Some(v.trim().trim_matches(['\"', '\'']).to_string())
                    } else {
                        None
                    }
                })
            }
            "grok" => {
                let cfg = self.settings_config.get("config").and_then(|v| v.as_str())?;
                cfg.lines().find_map(|l| {
                    let trimmed = l.trim();
                    let (k, v) = trimmed.split_once('=')?;
                    if k.trim() == "models_base_url" || k.trim() == "base_url" {
                        Some(v.trim().trim_matches(['\"', '\'']).to_string())
                    } else {
                        None
                    }
                })
            }
            _ => None,
        }
    }

    pub fn extract_api_key(&self, app: &str) -> Option<String> {
        match app {
            "claude" => {
                let env = self.settings_config.get("env")?;
                let key_field = self
                    .meta
                    .get("apiKeyField")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ANTHROPIC_AUTH_TOKEN");
                env.get(key_field)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            }
            "codex" => self
                .settings_config
                .get("auth")
                .and_then(|a| a.get("OPENAI_API_KEY"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            "grok" => {
                let auth_key = self
                    .settings_config
                    .get("auth")
                    .and_then(|a| a.get("GROK_API_KEY").or_else(|| a.get("XAI_API_KEY")))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if auth_key.is_some() {
                    return auth_key;
                }
                let cfg = self.settings_config.get("config").and_then(|v| v.as_str())?;
                cfg.lines().find_map(|l| {
                    let (k, v) = l.trim().split_once('=')?;
                    let k_trim = k.trim();
                    if k_trim == "api_key" || k_trim == "grok_api_key" || k_trim == "xai_api_key" {
                        Some(v.trim().trim_matches(['\"', '\'']).to_string())
                    } else {
                        None
                    }
                })
            }
            _ => None,
        }
    }

    pub fn extract_model(&self, app: &str) -> Option<String> {
        match app {
            "claude" => {
                let env = self.settings_config.get("env")?;
                env.get("ANTHROPIC_MODEL")
                    .or_else(|| env.get("ANTHROPIC_DEFAULT_SONNET_MODEL"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            }
            "codex" => {
                let cfg = self.settings_config.get("config").and_then(|v| v.as_str())?;
                cfg.lines().find_map(|l| {
                    let trimmed = l.trim();
                    let (k, v) = trimmed.split_once('=')?;
                    if k.trim() == "model" {
                        Some(v.trim().trim_matches(['\"', '\'']).to_string())
                    } else {
                        None
                    }
                })
            }
            "grok" => {
                let cfg = self.settings_config.get("config").and_then(|v| v.as_str())?;
                cfg.lines().find_map(|l| {
                    let trimmed = l.trim();
                    let (k, v) = trimmed.split_once('=')?;
                    if k.trim() == "model" {
                        Some(v.trim().trim_matches(['\"', '\'']).to_string())
                    } else {
                        None
                    }
                })
            }
            _ => None,
        }
    }

    pub fn extract_wire_api(&self, _app: &str) -> String {
        self.meta
            .get("wireApi")
            .and_then(|v| v.as_str())
            .unwrap_or("responses")
            .to_string()
    }

    pub fn extract_api_key_field(&self, app: &str) -> Option<String> {
        if app == "claude" {
            Some(
                self.meta
                    .get("apiKeyField")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ANTHROPIC_AUTH_TOKEN")
                    .to_string(),
            )
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
            settings: json!({
                "theme": "light",
                "autoLaunch": false,
                "backupBeforeWrite": true,
                "initialImportDone": false,
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
            }),
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
            data.order.retain(|id| data.providers.contains_key(id));
            if data
                .current
                .as_ref()
                .is_none_or(|id| !data.providers.contains_key(id))
            {
                data.current = Some(official_id);
                changed = true;
            }
        }

        if let Some(data) = self.apps.get_mut("codex") {
            for provider in data.providers.values_mut() {
                if is_official_provider(provider) {
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
        self.apps.values().any(|data| {
            data.providers
                .values()
                .any(|provider| !is_official_provider(provider))
        })
    }
}

/// 载入 providers.json；不存在则创建
pub fn load() -> Root {
    let path = config::get_store_path();
    if path.exists() {
        match config::read_json_file::<Root>(&path) {
            Ok(root) => return root,
            Err(e) => eprintln!("[z-switch] providers.json 解析失败，改用空数据: {e}"),
        }
    }
    let root = Root::default_seeded();
    let _ = save(&root);
    root
}

/// 原子保存 providers.json
pub fn save(root: &Root) -> Result<(), String> {
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

        assert_eq!(p.extract_base_url("claude").as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(p.extract_api_key("claude").as_deref(), Some("sk-123456"));
        assert_eq!(p.extract_model("claude").as_deref(), Some("deepseek-chat"));
        assert_eq!(p.extract_api_key_field("claude").as_deref(), Some("ANTHROPIC_AUTH_TOKEN"));
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

        assert_eq!(p.extract_base_url("codex").as_deref(), Some("https://open.bigmodel.cn/api/paas/v4"));
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
}

