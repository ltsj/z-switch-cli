//! 文件路径 + 原子读写。安全写盘核心。
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{Map, Value};
use std::fs;
#[cfg(windows)]
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct StoreLock {
    file: fs::File,
}

impl StoreLock {
    fn acquire() -> Result<Self, String> {
        let path = get_store_lock_path();
        let parent = path
            .parent()
            .ok_or_else(|| "无效的配置锁路径".to_string())?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("创建配置锁目录 {} 失败: {e}", parent.display()))?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("打开配置锁 {} 失败: {e}", path.display()))?;
        fs2::FileExt::lock_exclusive(&file)
            .map_err(|e| format!("获取配置锁 {} 失败: {e}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// 用户主目录。测试可用 `Z_SWITCH_TEST_HOME` 覆盖。
pub fn get_home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("Z_SWITCH_TEST_HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// z-switch 自身数据目录：~/.z-switch
pub fn get_app_config_dir() -> PathBuf {
    get_home_dir().join(".z-switch")
}

/// providers.json 路径
pub fn get_store_path() -> PathBuf {
    get_app_config_dir().join("providers.json")
}

pub fn get_store_lock_path() -> PathBuf {
    get_app_config_dir().join("providers.json.lock")
}

pub fn lock_store() -> Result<StoreLock, String> {
    StoreLock::acquire()
}

/// z-switch 管理的本机账号凭据快照目录。
pub fn get_official_account_dir() -> PathBuf {
    get_app_config_dir().join("official")
}

/// Claude Code 主配置：~/.claude/settings.json
pub fn get_claude_settings_path() -> PathBuf {
    get_home_dir().join(".claude").join("settings.json")
}

/// Codex auth：~/.codex/auth.json
pub fn get_codex_auth_path() -> PathBuf {
    get_home_dir().join(".codex").join("auth.json")
}

/// Codex config：~/.codex/config.toml
pub fn get_codex_config_path() -> PathBuf {
    get_home_dir().join(".codex").join("config.toml")
}

/// Grok config：~/.grok/config.toml
pub fn get_grok_config_path() -> PathBuf {
    get_home_dir().join(".grok").join("config.toml")
}

/// 创建仅当前用户可访问的 z-switch 数据目录。
///
/// 这些目录可能保存 API Key、登录令牌或包含请求上下文的错误日志；
/// Unix 上不能依赖用户的 umask 来保证已有目录的权限足够严格。
pub fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("创建私有数据目录 {} 失败: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("设置私有数据目录 {} 权限失败: {error}", path.display()))?;
    }
    Ok(())
}

/// 递归按字母排序对象的键，保证序列化输出确定性。
fn sort_json_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = Map::new();
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for k in keys {
                sorted.insert(k.clone(), sort_json_keys(&map[k]));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_json_keys).collect()),
        other => other.clone(),
    }
}

/// 读取并反序列化 JSON 文件
pub fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let content =
        fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    serde_json::from_str(&content).map_err(|e| format!("解析 {} 失败: {e}", path.display()))
}

/// 序列化并原子写入 JSON 文件（键排序，确定性输出）
pub fn write_json_file<T: Serialize>(path: &Path, data: &T) -> Result<(), String> {
    let value = serde_json::to_value(data).map_err(|e| e.to_string())?;
    let sorted = sort_json_keys(&value);
    let json = serde_json::to_string_pretty(&sorted).map_err(|e| e.to_string())?;
    atomic_write(path, json.as_bytes())
}

/// 原子写入文本文件
pub fn write_text_file(path: &Path, data: &str) -> Result<(), String> {
    atomic_write(path, data.as_bytes())
}

/// 原子写入：写临时文件（带纳秒后缀）→ 原子替换，避免半写状态。
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "无效的路径（无父目录）".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;

    let file_name = path
        .file_name()
        .ok_or_else(|| "无效的文件名".to_string())?
        .to_string_lossy()
        .to_string();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = parent.join(format!("{file_name}.tmp.{ts}"));

    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Managed files contain API keys and are replaced through a new
            // inode. Do not inherit an accidentally broad mode from an old
            // providers.json/settings file.
            options.mode(0o600);
        }
        let mut f = options
            .open(&tmp)
            .map_err(|e| format!("创建临时文件失败: {e}"))?;
        if let Err(error) = f.write_all(data) {
            drop(f);
            let _ = fs::remove_file(&tmp);
            return Err(format!("写入临时文件失败: {error}"));
        }
        if let Err(error) = f.flush() {
            drop(f);
            let _ = fs::remove_file(&tmp);
            return Err(format!("flush 失败: {error}"));
        }
    }

    #[cfg(windows)]
    let result = replace_file_windows(&tmp, path);
    #[cfg(not(windows))]
    let result = fs::rename(&tmp, path);

    result.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("原子替换失败 {} -> {}: {e}", tmp.display(), path.display())
    })
}

#[cfg(windows)]
fn replace_file_windows(tmp: &Path, path: &Path) -> io::Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    fn wide(path: &Path) -> Vec<u16> {
        OsStr::new(path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let tmp_wide = wide(tmp);
    let path_wide = wide(path);
    if path.exists() {
        let replaced = unsafe {
            ReplaceFileW(
                path_wide.as_ptr(),
                tmp_wide.as_ptr(),
                std::ptr::null(),
                REPLACE_FILE_WRITE_THROUGH,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if replaced != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    } else {
        let moved = unsafe {
            MoveFileExW(
                tmp_wide.as_ptr(),
                path_wide.as_ptr(),
                MOVE_FILE_WRITE_THROUGH,
            )
        };
        if moved != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(windows)]
const REPLACE_FILE_WRITE_THROUGH: u32 = 0x00000001;
#[cfg(windows)]
const MOVE_FILE_WRITE_THROUGH: u32 = 0x00000008;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn ReplaceFileW(
        replaced_file_name: *const u16,
        replacement_file_name: *const u16,
        backup_file_name: *const u16,
        replace_flags: u32,
        exclude: *mut std::ffi::c_void,
        reserved: *mut std::ffi::c_void,
    ) -> i32;
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sort_json_keys() {
        let unsorted = serde_json::json!({
            "z": 1,
            "a": 2,
            "m": {
                "sub_b": 10,
                "sub_a": 20
            }
        });
        let sorted = sort_json_keys(&unsorted);
        let serialized = serde_json::to_string(&sorted).unwrap();
        assert_eq!(serialized, r#"{"a":2,"m":{"sub_a":20,"sub_b":10},"z":1}"#);
    }

    #[test]
    fn test_atomic_write() {
        let dir = std::env::temp_dir().join(format!("z_switch_test_{}", std::process::id()));
        let file = dir.join("test.txt");
        let content = b"hello z-switch atomic write";

        let res = atomic_write(&file, content);
        assert!(res.is_ok());
        let read = fs::read(&file).unwrap();
        assert_eq!(read, content);

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_data_directories_are_restricted() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "z_switch_private_dir_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_dir(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_write_removes_temporary_file_when_replace_fails() {
        let dir = std::env::temp_dir().join(format!(
            "z_switch_atomic_failure_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        fs::create_dir(&target).unwrap();

        assert!(atomic_write(&target, b"secret").is_err());
        let temporary_files = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("target.tmp.")
            })
            .count();
        assert_eq!(temporary_files, 0);

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn new_atomic_files_are_private_by_default() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "z_switch_private_test_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let file = dir.join("credentials.json");
        atomic_write(&file, b"secret").unwrap();
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replace_tightens_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "z_switch_permission_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = dir.join("credentials.json");
        atomic_write(&file, b"old").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write(&file, b"new").unwrap();

        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read(&file).unwrap(), b"new");
        let _ = fs::remove_dir_all(dir);
    }
}
