//! 代理常驻后台守护进程管理器与 IPC 客户端。
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::proxy::{
    self, AdminHealthResponse, AdminStatusResponse, AdminSwitchRequest, AdminSwitchResponse,
    AppTarget, ProxyControl, ProxyHandle, ProxyRuntimeConfig,
};
use crate::store;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PidInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: u64,
}

pub fn pid_file_path() -> PathBuf {
    config::get_app_config_dir().join("proxy.pid")
}

pub fn read_pid_file() -> Option<PidInfo> {
    let path = pid_file_path();
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn write_pid_file(port: u16) -> Result<(), String> {
    let path = pid_file_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let info = PidInfo {
        pid: std::process::id(),
        port,
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let json = serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?;
    config::atomic_write(&path, json.as_bytes())
}

pub fn remove_pid_file() {
    let _ = fs::remove_file(pid_file_path());
}

// ---------------- IPC 客户端 ----------------

fn http_client(timeout_ms: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(timeout_ms))
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .unwrap_or_default()
}

pub async fn check_health(port: u16) -> Result<AdminHealthResponse, String> {
    let url = format!("http://127.0.0.1:{port}/_admin/health");
    let resp = http_client(1200)
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("代理未响应: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("健康检查状态码异常: {}", resp.status()));
    }
    resp.json::<AdminHealthResponse>()
        .await
        .map_err(|e| format!("解析健康响应失败: {e}"))
}

pub async fn get_status(port: u16) -> Result<AdminStatusResponse, String> {
    let url = format!("http://127.0.0.1:{port}/_admin/status");
    let resp = http_client(2000)
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("获取状态失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("状态接口响应异常: {}", resp.status()));
    }
    resp.json::<AdminStatusResponse>()
        .await
        .map_err(|e| format!("解析状态响应失败: {e}"))
}

pub async fn send_switch(
    port: u16,
    app: &str,
    target: Option<AppTarget>,
) -> Result<AdminSwitchResponse, String> {
    let url = format!("http://127.0.0.1:{port}/_admin/switch");
    let payload = AdminSwitchRequest {
        app: app.to_string(),
        target,
    };
    let resp = http_client(3000)
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("发送热切换信令失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("热切接口响应异常: {}", resp.status()));
    }
    resp.json::<AdminSwitchResponse>()
        .await
        .map_err(|e| format!("解析热切响应失败: {e}"))
}

pub async fn send_shutdown(port: u16) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/_admin/shutdown");
    let _ = http_client(2000).post(&url).send().await;
    Ok(())
}

// ---------------- 守护进程启停逻辑 ----------------

pub async fn is_running(port: u16) -> bool {
    check_health(port).await.is_ok()
}

pub async fn start_background(port: u16) -> Result<(), String> {
    if is_running(port).await {
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|e| format!("获取当前程序路径失败: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["proxy", "worker", "--port", &port.to_string()]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const DETACHED_PROCESS: u32 = 0x00000008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let _child = cmd.spawn().map_err(|e| format!("拉起后台代理进程失败: {e}"))?;

    // 等待就绪
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if is_running(port).await {
            return Ok(());
        }
    }
    Err(format!("后台代理进程启动超时（5秒内未响应端口 {port}）"))
}

pub async fn stop(port: u16) -> Result<(), String> {
    if !is_running(port).await {
        remove_pid_file();
        return Ok(());
    }

    let _ = send_shutdown(port).await;

    // 等待退出
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !is_running(port).await {
            remove_pid_file();
            return Ok(());
        }
    }

    // 强杀兜底
    if let Some(info) = read_pid_file() {
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/PID", &info.pid.to_string()])
                .output();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-9", &info.pid.to_string()])
                .output();
        }
    }
    remove_pid_file();
    Ok(())
}

/// 后台常驻工作进程入口 (由 `z-switch proxy worker --port <port>` 启动)
pub async fn run_worker(port: u16) -> Result<(), String> {
    let root = store::load();
    let runtime_cfg = ProxyRuntimeConfig::from_settings(&root.settings);

    let handle = ProxyHandle::default();
    // 注入已选中的供应商
    for &app in proxy::PROXY_APPS {
        if let Some(app_data) = root.apps.get(app) {
            if let Some(cur_id) = &app_data.current {
                if let Some(provider) = app_data.providers.get(cur_id) {
                    if !store::is_official_provider(provider) {
                        if let Some(target) = proxy::target_from_provider(app, provider) {
                            proxy::set_target(&handle.targets, app, target);
                        }
                    }
                }
            }
        }
    }

    let mut control = ProxyControl::new(handle);
    control.start(port, runtime_cfg).await?;

    if let Err(e) = write_pid_file(port) {
        eprintln!("[z-switch] 写入 PID 文件警告: {e}");
    }

    // 监听 Ctrl+C 或等待优雅停机
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !control.handle.is_running() {
                    break;
                }
            }
        } => {}
    }

    control.stop();
    remove_pid_file();
    Ok(())
}
