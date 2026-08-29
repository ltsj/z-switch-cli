#![allow(dead_code)]
//! Claude Code VS Code 扩展放行与首次启动引导控制。
use crate::config;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

fn claude_config_path() -> PathBuf {
    config::get_home_dir().join(".claude").join("config.json")
}

fn claude_json_path() -> PathBuf {
    config::get_home_dir().join(".claude.json")
}

fn read_obj_or_empty(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let value: Value = config::read_json_file(path)?;
    match value {
        Value::Object(map) => Ok(map),
        Value::Null => Ok(Map::new()),
        _ => Err(format!("{} 不是 JSON 对象，已跳过", path.display())),
    }
}

pub fn apply_primary_api_key(managed: bool) -> Result<(), String> {
    let path = claude_config_path();
    let mut obj = read_obj_or_empty(&path)?;
    if managed {
        obj.insert("primaryApiKey".into(), Value::String("any".into()));
    } else if obj.remove("primaryApiKey").is_none() {
        if !path.exists() {
            return Ok(());
        }
    }
    config::write_json_file(&path, &Value::Object(obj))
}

pub fn apply_onboarding_completed(enabled: bool) -> Result<(), String> {
    let path = claude_json_path();
    let mut obj = read_obj_or_empty(&path)?;
    if enabled {
        obj.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    } else if obj.remove("hasCompletedOnboarding").is_none() {
        if !path.exists() {
            return Ok(());
        }
    }
    config::write_json_file(&path, &Value::Object(obj))
}
