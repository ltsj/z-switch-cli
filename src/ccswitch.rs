//! 从 cc-switch 导入供应商。
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::config;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CcswitchProvider {
    pub app: String,
    pub name: String,
    pub settings_config: Value,
    #[serde(default)]
    pub meta: Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CcswitchScan {
    pub source: String,
    pub providers: Vec<CcswitchProvider>,
}

const SUPPORTED_APPS: &[&str] = &["claude", "codex"];

fn is_supported_app(app: &str) -> bool {
    SUPPORTED_APPS.contains(&app)
}

fn ccswitch_dir() -> PathBuf {
    config::get_home_dir().join(".cc-switch")
}

fn has_base_url(app: &str, cfg: &Value) -> bool {
    match app {
        "claude" => cfg
            .get("env")
            .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        "codex" => cfg
            .get("config")
            .and_then(|v| v.as_str())
            .and_then(|toml| crate::store::extract_codex_provider_string(toml, "base_url"))
            .is_some_and(|value| !value.trim().is_empty()),
        _ => false,
    }
}

fn has_proxy_placeholder(app: &str, cfg: &Value) -> bool {
    match app {
        "claude" => cfg
            .get("env")
            .and_then(Value::as_object)
            .is_some_and(|env| {
                ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
                    .iter()
                    .any(|key| {
                        env.get(*key)
                            .and_then(Value::as_str)
                            .is_some_and(crate::proxy::is_placeholder_key)
                    })
            }),
        "codex" => cfg
            .get("auth")
            .and_then(|auth| auth.get("OPENAI_API_KEY"))
            .and_then(Value::as_str)
            .is_some_and(crate::proxy::is_placeholder_key),
        _ => false,
    }
}

pub fn scan() -> Result<CcswitchScan, String> {
    let dir = ccswitch_dir();
    let db = dir.join("cc-switch.db");
    let json = dir.join("config.json");

    if db.exists() {
        match scan_sqlite(&db) {
            Ok(providers) => {
                return Ok(CcswitchScan {
                    source: "sqlite".into(),
                    providers,
                })
            }
            Err(error) => {
                eprintln!("[z-switch] 读取 cc-switch.db 失败，尝试 config.json: {error}");
            }
        }
    }
    if json.exists() {
        return Ok(CcswitchScan {
            source: "json".into(),
            providers: scan_json(&json)?,
        });
    }
    Ok(CcswitchScan {
        source: "none".into(),
        providers: vec![],
    })
}

fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| e.to_string())?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(cols)
}

fn scan_sqlite(db_path: &Path) -> Result<Vec<CcswitchProvider>, String> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("打开 cc-switch.db 失败: {e}"))?;

    let cols = table_columns(&conn, "providers")?;
    if cols.is_empty() {
        return Err("cc-switch.db 未找到 providers 表".into());
    }
    let has = |c: &str| cols.iter().any(|x| x == c);
    if !has("app_type") || !has("settings_config") {
        return Err("cc-switch.db 的 providers 表结构不兼容".into());
    }
    let name_col = if has("name") { "name" } else { "id" };
    let with_meta = has("meta");

    let sql = format!(
        "SELECT app_type, {name_col}, settings_config{meta} FROM providers",
        meta = if with_meta { ", meta" } else { "" },
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("查询 providers 失败: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let app: Option<String> = row.get(0)?;
            let name: Option<String> = row.get(1)?;
            let settings: Option<String> = row.get(2)?;
            let meta: Option<String> = if with_meta {
                row.get(3).unwrap_or(None)
            } else {
                None
            };
            Ok((app, name, settings, meta))
        })
        .map_err(|e| format!("读取 providers 失败: {e}"))?;

    let mut out = Vec::new();
    for row in rows {
        let (app, name, settings, meta) = match row {
            Ok(row) => row,
            Err(error) => {
                eprintln!("[z-switch] 跳过无法读取的 cc-switch 供应商行：{error}");
                continue;
            }
        };
        let Some(app) = app else {
            continue;
        };
        if !is_supported_app(&app) {
            continue;
        }
        let Some(settings) = settings else {
            continue;
        };
        let Ok(settings_config) = serde_json::from_str::<Value>(&settings) else {
            continue;
        };
        if !has_base_url(&app, &settings_config) || has_proxy_placeholder(&app, &settings_config) {
            continue;
        }
        let meta = meta
            .and_then(|m| serde_json::from_str::<Value>(&m).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let name = if name.as_deref().is_none_or(|name| name.trim().is_empty()) {
            app.clone()
        } else {
            name.unwrap_or_default()
        };
        out.push(CcswitchProvider {
            app,
            name,
            settings_config,
            meta,
        });
    }
    Ok(out)
}

fn scan_json(path: &Path) -> Result<Vec<CcswitchProvider>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 cc-switch config.json 失败: {e}"))?;
    let root: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 cc-switch config.json 失败: {e}"))?;

    let mut out = Vec::new();
    for app in SUPPORTED_APPS {
        let app_node = root
            .get("providers")
            .and_then(|p| p.get(app))
            .or_else(|| root.get("apps").and_then(|p| p.get(app)));
        let Some(app_node) = app_node else {
            continue;
        };
        let Some(map) = app_node.get("providers").and_then(|v| v.as_object()) else {
            continue;
        };
        for provider in map.values() {
            let name = provider
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.trim().is_empty() {
                continue;
            }
            let settings_config = provider
                .get("settingsConfig")
                .or_else(|| provider.get("settings_config"))
                .cloned()
                .unwrap_or(Value::Null);
            if settings_config.is_null()
                || !has_base_url(app, &settings_config)
                || has_proxy_placeholder(app, &settings_config)
            {
                continue;
            }
            let meta = provider
                .get("meta")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            out.push(CcswitchProvider {
                app: (*app).to_string(),
                name,
                settings_config,
                meta,
            });
        }
    }
    Ok(out)
}

pub fn import_from_sqlite_path(
    service: &crate::service::SwitchService,
    path: &Path,
) -> Result<usize, String> {
    let providers = scan_sqlite(path)?;
    import_ccswitch_providers(service, providers)
}

pub fn import_from_json_path(
    service: &crate::service::SwitchService,
    path: &Path,
) -> Result<usize, String> {
    let providers = scan_json(path)?;
    import_ccswitch_providers(service, providers)
}

pub fn import_auto(service: &crate::service::SwitchService) -> Result<usize, String> {
    let scan_res = scan()?;
    if scan_res.providers.is_empty() {
        return Err("未在 ~/.cc-switch 找到可导入的有效供应商".into());
    }
    import_ccswitch_providers(service, scan_res.providers)
}

fn import_ccswitch_providers(
    service: &crate::service::SwitchService,
    list: Vec<CcswitchProvider>,
) -> Result<usize, String> {
    let mut count = 0;
    let mut failures = Vec::new();
    for item in list {
        let name = item.name.clone();
        let base_id = item
            .name
            .to_lowercase()
            .replace(|c: char| !c.is_alphanumeric(), "-");
        let id = format!("cc-{base_id}");
        let provider = crate::store::Provider {
            id,
            name: item.name,
            category: Some("imported".into()),
            settings_config: item.settings_config,
            meta: item.meta,
            failover: serde_json::json!({ "enabled": false }),
        };
        match service.add_provider(&item.app, provider) {
            Ok(_) => count += 1,
            Err(error) => failures.push(format!("{name} ({})：{error}", item.app)),
        }
    }
    for failure in failures {
        eprintln!("[z-switch] 跳过无效 cc-switch 供应商：{failure}");
    }
    if count == 0 {
        return Err("找到的 cc-switch 供应商均无法导入".into());
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::scan_sqlite;
    use rusqlite::params;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sqlite_nullable_provider_fields_do_not_abort_the_scan() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "z_switch_ccswitch_nullable_{}_{}.db",
            std::process::id(),
            suffix
        ));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE providers (app_type TEXT, name TEXT, settings_config TEXT, meta TEXT)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO providers (app_type, name, settings_config, meta) VALUES (?1, NULL, ?2, NULL)",
                params![
                    "claude",
                    r#"{"env":{"ANTHROPIC_BASE_URL":"https://relay.example"}}"#
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO providers (app_type, name, settings_config, meta) VALUES (?1, ?2, NULL, NULL)",
                params!["codex", "missing settings"],
            )
            .unwrap();
        }

        let providers = scan_sqlite(&path).unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "claude");
        let _ = std::fs::remove_file(path);
    }
}
