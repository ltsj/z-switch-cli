//! Claude 桌面版（独立聊天 App）3p 网关配置写盘。
use crate::config::{atomic_write, read_json_file, write_json_file};
use crate::store::Provider;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const PROFILE_ID: &str = "00000000-0000-4000-8000-007a53000001";
pub const PROFILE_NAME: &str = "z-switch";

#[cfg(any(target_os = "macos", windows, test))]
const CONFIG_FILE: &str = "claude_desktop_config.json";
#[cfg(any(target_os = "macos", windows, test))]
const CONFIG_LIBRARY_DIR: &str = "configLibrary";

const GATEWAY_TOKEN: &str = "z-switch-desktop";

const ENTERPRISE_KEYS: &[&str] = &[
    "disableDeploymentModeChooser",
    "inferenceGatewayApiKey",
    "inferenceGatewayAuthScheme",
    "inferenceGatewayBaseUrl",
    "inferenceProvider",
];

const DEFAULT_ROUTES: &[(&str, bool)] = &[
    ("claude-sonnet-4-6", false),
    ("claude-opus-4-8", true),
    ("claude-haiku-4-5", false),
    ("claude-fable-5", false),
];

#[derive(Debug, Clone)]
struct Paths {
    normal_config: PathBuf,
    threep_config: PathBuf,
    profile: PathBuf,
    meta: PathBuf,
}

struct FileSnapshot {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

pub fn is_supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

fn desktop_app_present(paths: &Paths) -> bool {
    paths.normal_config.parent().is_some_and(Path::exists)
        || paths.threep_config.parent().is_some_and(Path::exists)
}

pub fn apply_direct(provider: &Provider) -> Result<(), String> {
    let (base_url, api_key) = direct_credentials(provider)?;
    apply_gateway(&base_url, &api_key)
}

pub fn apply_proxy(local_base: &str) -> Result<(), String> {
    apply_gateway(local_base, GATEWAY_TOKEN)
}

fn apply_gateway(base_url: &str, api_key: &str) -> Result<(), String> {
    let paths = current_paths()?;
    if !desktop_app_present(&paths) {
        return Ok(());
    }
    let profile = build_profile(base_url, api_key);
    with_rollback(&paths, |paths| {
        write_deployment_mode(&paths.normal_config, "3p")?;
        write_deployment_mode(&paths.threep_config, "3p")?;
        write_json_file(&paths.profile, &profile)?;
        write_meta(&paths.meta, Some(PROFILE_ID))
    })
}

pub fn restore_official() -> Result<(), String> {
    let paths = current_paths()?;
    if !desktop_app_present(&paths) {
        return Ok(());
    }
    with_rollback(&paths, |paths| {
        write_deployment_mode(&paths.normal_config, "1p")?;
        write_deployment_mode(&paths.threep_config, "1p")?;
        remove_enterprise_config(&paths.threep_config)?;
        if paths.profile.exists() {
            fs::remove_file(&paths.profile)
                .map_err(|e| format!("删除 {} 失败: {e}", paths.profile.display()))?;
        }
        write_meta(&paths.meta, None)
    })
}

fn build_profile(base_url: &str, api_key: &str) -> Value {
    let models: Vec<Value> = DEFAULT_ROUTES
        .iter()
        .map(|(id, one_m)| {
            let mut m = json!({ "name": id, "labelOverride": id });
            if *one_m {
                m["supports1m"] = json!(true);
            }
            m
        })
        .collect();
    json!({
        "coworkEgressAllowedHosts": ["*"],
        "disableDeploymentModeChooser": true,
        "inferenceGatewayApiKey": api_key,
        "inferenceGatewayAuthScheme": "bearer",
        "inferenceGatewayBaseUrl": base_url,
        "inferenceModels": models,
        "inferenceProvider": "gateway"
    })
}

fn direct_credentials(provider: &Provider) -> Result<(String, String), String> {
    let base_url = provider
        .extract_base_url("claude")
        .map(|value| value.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or("供应商缺少 ANTHROPIC_BASE_URL")?;
    let api_key = provider
        .extract_api_key("claude")
        .map(|value| value.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or("供应商缺少 Claude API Key")?;
    Ok((base_url, api_key))
}

fn read_obj_or_empty(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    match read_json_file::<Value>(path) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(Value::Null) => Err(format!("{} 不是 JSON 对象，已中止写入", path.display())),
        Ok(_) => Err(format!("{} 不是 JSON 对象，已中止写入", path.display())),
        Err(error) => Err(error),
    }
}

fn write_deployment_mode(path: &Path, mode: &str) -> Result<(), String> {
    let mut obj = read_obj_or_empty(path)?;
    obj.insert("deploymentMode".into(), Value::String(mode.into()));
    write_json_file(path, &Value::Object(obj))
}

fn write_meta(path: &Path, applied_profile_id: Option<&str>) -> Result<(), String> {
    let mut obj = read_obj_or_empty(path)?;
    let mut entries = match obj.get("entries") {
        None => Vec::new(),
        Some(Value::Array(entries)) => entries.clone(),
        Some(_) => {
            return Err(format!(
                "{} 的 entries 不是 JSON 数组，已中止写入",
                path.display()
            ))
        }
    };
    entries.retain(|e| e.get("id").and_then(Value::as_str) != Some(PROFILE_ID));

    match applied_profile_id {
        Some(id) => {
            entries.push(json!({ "id": PROFILE_ID, "name": PROFILE_NAME }));
            obj.insert("appliedId".into(), Value::String(id.into()));
        }
        None => {
            let ours = obj
                .get("appliedId")
                .and_then(Value::as_str)
                .is_some_and(|id| id == PROFILE_ID);
            if ours {
                match entries
                    .iter()
                    .find_map(|e| e.get("id").and_then(Value::as_str))
                {
                    Some(next) => {
                        obj.insert("appliedId".into(), Value::String(next.into()));
                    }
                    None => {
                        obj.remove("appliedId");
                    }
                }
            }
        }
    }

    obj.insert("entries".into(), Value::Array(entries));
    write_json_file(path, &Value::Object(obj))
}

fn remove_enterprise_config(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut obj = read_obj_or_empty(path)?;
    let Some(enterprise) = obj
        .get_mut("enterpriseConfig")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };
    for key in ENTERPRISE_KEYS {
        enterprise.remove(*key);
    }
    if enterprise.is_empty() {
        obj.remove("enterpriseConfig");
    }
    write_json_file(path, &Value::Object(obj))
}

fn with_rollback<F>(paths: &Paths, op: F) -> Result<(), String>
where
    F: FnOnce(&Paths) -> Result<(), String>,
{
    let snapshots = snapshot(paths)?;
    match op(paths) {
        Ok(()) => Ok(()),
        Err(err) => match restore(&snapshots) {
            Ok(()) => Err(err),
            Err(rollback_err) => Err(format!("{err}; 回滚也失败: {rollback_err}")),
        },
    }
}

fn snapshot(paths: &Paths) -> Result<Vec<FileSnapshot>, String> {
    [
        &paths.normal_config,
        &paths.threep_config,
        &paths.profile,
        &paths.meta,
    ]
    .into_iter()
    .map(|path| {
        let content = if path.exists() {
            Some(fs::read(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?)
        } else {
            None
        };
        Ok(FileSnapshot {
            path: path.clone(),
            content,
        })
    })
    .collect()
}

fn restore(snapshots: &[FileSnapshot]) -> Result<(), String> {
    let mut applied: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    for snap in snapshots {
        let previous = match fs::read(&snap.path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "读取 {} 当前状态失败: {error}",
                    snap.path.display()
                ));
            }
        };
        let result = match &snap.content {
            Some(content) => atomic_write(&snap.path, content),
            None => match fs::remove_file(&snap.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("删除 {} 失败: {error}", snap.path.display())),
            },
        };
        if let Err(error) = result {
            let mut rollback_errors = Vec::new();
            for (path, content) in applied.iter().rev() {
                let rollback = match content {
                    Some(content) => atomic_write(path, content),
                    None => match fs::remove_file(path) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(error) => Err(format!("删除 {} 失败: {error}", path.display())),
                    },
                };
                if let Err(rollback_error) = rollback {
                    rollback_errors.push(format!("恢复 {} 失败: {rollback_error}", path.display()));
                }
            }
            if rollback_errors.is_empty() {
                return Err(format!("{error}；已回滚已恢复的桌面配置文件"));
            }
            return Err(format!(
                "{error}；桌面配置文件回滚失败: {}",
                rollback_errors.join("；")
            ));
        }
        applied.push((snap.path.clone(), previous));
    }
    Ok(())
}

#[allow(clippy::needless_return)]
fn current_paths() -> Result<Paths, String> {
    #[cfg(target_os = "macos")]
    {
        let app_support = crate::config::get_home_dir()
            .join("Library")
            .join("Application Support");
        return Ok(paths_from_dirs(
            app_support.join("Claude"),
            app_support.join("Claude-3p"),
        ));
    }

    #[cfg(windows)]
    {
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::config::get_home_dir().join("AppData").join("Local"));
        let normal = pick_windows_claude_dir(&local_app_data, false)
            .unwrap_or_else(|| local_app_data.join("Claude"));
        let threep = pick_windows_claude_dir(&local_app_data, true)
            .unwrap_or_else(|| local_app_data.join("Claude-3p"));
        return Ok(paths_from_dirs(normal, threep));
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err("当前平台不支持 Claude 桌面版 3p 配置（仅 macOS / Windows）".into())
    }
}

#[cfg(windows)]
fn pick_windows_claude_dir(local_app_data: &Path, threep: bool) -> Option<PathBuf> {
    let exact = local_app_data.join(if threep { "Claude-3p" } else { "Claude" });
    if exact.exists() {
        return Some(exact);
    }
    let mut candidates: Vec<PathBuf> = fs::read_dir(local_app_data)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            name.starts_with("Claude") && name.contains("-3p") == threep
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

#[cfg(any(target_os = "macos", windows, test))]
fn paths_from_dirs(normal_dir: PathBuf, threep_dir: PathBuf) -> Paths {
    let library = threep_dir.join(CONFIG_LIBRARY_DIR);
    Paths {
        normal_config: normal_dir.join(CONFIG_FILE),
        threep_config: threep_dir.join(CONFIG_FILE),
        profile: library.join(format!("{PROFILE_ID}.json")),
        meta: library.join("_meta.json"),
    }
}
