//! 环境自检：检测 live 配置里的「本地代理占位残留」。
use serde_json::Value;
use std::fs;
use std::net::IpAddr;

use crate::config;

pub struct LiveSnapshot {
    pub base_url: Option<String>,
    pub key_is_placeholder: bool,
}

pub fn is_localhost(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url.trim()) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host == "localhost" {
        return true;
    }
    let Ok(ip) = host.parse::<IpAddr>() else {
        return false;
    };
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
    }
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
            ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
                .iter()
                .any(|k| {
                    e.get(*k)
                        .and_then(|v| v.as_str())
                        .map(crate::proxy::is_placeholder_key)
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
        .and_then(|cfg| crate::store::extract_codex_provider_string(&cfg, "base_url"));
    let key_is_placeholder = fs::read_to_string(config::get_codex_auth_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("OPENAI_API_KEY")
                .and_then(|k| k.as_str())
                .map(crate::proxy::is_placeholder_key)
        })
        .unwrap_or(false);
    LiveSnapshot {
        base_url,
        key_is_placeholder,
    }
}

pub fn read_grok() -> LiveSnapshot {
    fs::read_to_string(config::get_grok_config_path())
        .ok()
        .map(|cfg| parse_grok_snapshot(&cfg))
        .unwrap_or(LiveSnapshot {
            base_url: None,
            key_is_placeholder: false,
        })
}

fn parse_grok_snapshot(cfg: &str) -> LiveSnapshot {
    let base_url = crate::store::extract_grok_endpoint_string(cfg, "models_base_url")
        .or_else(|| crate::store::extract_grok_endpoint_string(cfg, "base_url"));
    let key_is_placeholder = ["api_key", "grok_api_key", "xai_api_key"]
        .into_iter()
        .filter_map(|key| crate::store::extract_grok_model_string(cfg, key))
        .any(|value| crate::proxy::is_placeholder_key(&value));
    LiveSnapshot {
        base_url,
        key_is_placeholder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_detection_matches_hosts_instead_of_substrings() {
        assert!(is_localhost("http://127.0.0.1:8999/claude"));
        assert!(is_localhost("http://127.42.0.8:8999/codex"));
        assert!(is_localhost("http://localhost:8999/grok"));
        assert!(is_localhost("http://localhost.:8999/grok"));
        assert!(is_localhost("http://[::1]:8999/grok"));
        assert!(is_localhost("http://[::ffff:127.0.0.1]:8999/grok"));
        assert!(!is_localhost("https://127.0.0.1.example.com/v1"));
        assert!(!is_localhost("https://example.com/localhost/v1"));
        assert!(!is_localhost("127.0.0.1:8999"));
    }

    #[test]
    fn grok_snapshot_does_not_read_an_inactive_model() {
        let config = r#"
[models]
default = "active"

[endpoints]
models_base_url = "https://api.example.com/v1"

[model."active"]
api_key = "real-key"

[model."inactive"]
base_url = "http://127.0.0.1:8999/grok"
api_key = "z-switch-proxy"
"#;

        let snapshot = parse_grok_snapshot(config);
        assert_eq!(
            snapshot.base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert!(!snapshot.key_is_placeholder);
    }
}
