//! 环境自检：检测 live 配置里的「本地代理占位残留」。
use serde_json::Value;
use std::fs;

use crate::config;
use crate::proxy::PLACEHOLDER_KEY;

pub struct LiveSnapshot {
    pub base_url: Option<String>,
    pub key_is_placeholder: bool,
}

pub fn is_localhost(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u.contains("127.0.0.1") || u.contains("localhost") || u.contains("[::1]")
}

pub fn read_claude() -> LiveSnapshot {
    let env = fs::read_to_string(config::get_claude_settings_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("env").cloned());
    let base_url = env
        .as_ref()
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let key_is_placeholder = env
        .as_ref()
        .map(|e| {
            ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"].iter().any(|k| {
                e.get(*k)
                    .and_then(|v| v.as_str())
                    .map(|s| s == PLACEHOLDER_KEY)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    LiveSnapshot {
        base_url,
        key_is_placeholder,
    }
}

pub fn read_codex() -> LiveSnapshot {
    let base_url = fs::read_to_string(config::get_codex_config_path())
        .ok()
        .and_then(|cfg| {
            cfg.lines()
                .find_map(|l| l.trim().strip_prefix("base_url"))
                .and_then(|r| r.split('"').nth(1))
                .map(|s| s.to_string())
        });
    let key_is_placeholder = fs::read_to_string(config::get_codex_auth_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("OPENAI_API_KEY")
                .and_then(|k| k.as_str())
                .map(|s| s == PLACEHOLDER_KEY)
        })
        .unwrap_or(false);
    LiveSnapshot {
        base_url,
        key_is_placeholder,
    }
}
