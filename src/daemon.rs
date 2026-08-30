//! 代理常驻后台守护进程管理器与 IPC 客户端。
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::live;
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
    #[serde(default)]
    pub admin_token: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StartOutcome {
    pub pid: u32,
    pub started: bool,
}

pub fn pid_file_path_for(port: u16) -> PathBuf {
    let file_name = if port == proxy::DEFAULT_PORT {
        "proxy.pid".to_string()
    } else {
        format!("proxy-{port}.pid")
    };
    config::get_app_config_dir().join(file_name)
}

fn start_lock_path_for(port: u16) -> PathBuf {
    config::get_app_config_dir().join(format!("proxy-{port}.start.lock"))
}

fn legacy_pid_file_path() -> PathBuf {
    config::get_app_config_dir().join("proxy.pid")
}

pub(crate) struct LifecycleLock {
    file: fs::File,
}

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

pub(crate) async fn acquire_lifecycle_lock(port: u16) -> Result<LifecycleLock, String> {
    let path = start_lock_path_for(port);
    let parent = path
        .parent()
        .ok_or_else(|| "无效的代理启动锁路径".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("创建代理启动锁目录 {} 失败: {error}", parent.display()))?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("打开代理启动锁 {} 失败: {error}", path.display()))?;

    let started = Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(LifecycleLock { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= Duration::from_secs(6) {
                    return Err(format!("等待代理启动锁超时: {}", path.display()));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(format!("获取代理启动锁 {} 失败: {error}", path.display()));
            }
        }
    }
}

pub fn validate_port(port: u16) -> Result<(), String> {
    if port == 0 {
        Err("代理端口必须在 1 到 65535 之间".into())
    } else {
        Ok(())
    }
}

/// 返回当前应用对应的 CLI 代理端口。
///
/// live 配置也可能来自 GUI；只有存在当前 CLI 自己的 PID 文件时，才把
/// live 里的 localhost 端口视为 CLI 代理，避免把 GUI 或其它进程误当成 CLI。
pub fn preferred_port_for_app(app: &str) -> Option<u16> {
    let port = live::proxy_port(app)?;
    // 端口号本身不能证明归 CLI 所有。PID 文件仍兼容旧版无 token 的格式，
    // 但真正的控制操作还会通过健康接口的 PID 和状态接口再次确认。
    read_pid_file(port).map(|_| port)
}

/// 返回 TUI/诊断可管理的 CLI 代理端口。
/// 默认端口始终保留，额外端口从 PID 文件中发现；PID 文件即使对应残留
/// 进程也保留在列表里，方便用户通过 status/stop 清理现场。
pub fn known_cli_ports() -> Vec<u16> {
    let mut ports = vec![proxy::DEFAULT_PORT];
    let dir = config::get_app_config_dir();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let port = if name == "proxy.pid" {
                Some(proxy::DEFAULT_PORT)
            } else {
                name.strip_prefix("proxy-")
                    .and_then(|value| value.strip_suffix(".pid"))
                    .and_then(|value| value.parse::<u16>().ok())
            };
            if let Some(port) = port.filter(|port| *port != 0) {
                ports.push(port);
            }
            if name == "proxy.pid" {
                // v0.3.1 及更早版本无论监听哪个端口都写入 proxy.pid。
                // 读取其中的端口，避免升级后 TUI 漏掉旧的自定义代理。
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    if let Ok(info) = serde_json::from_str::<PidInfo>(&content) {
                        if info.port != 0 {
                            ports.push(info.port);
                        }
                    }
                }
            }
        }
    }
    for app in proxy::PROXY_APPS {
        if let Some(port) = preferred_port_for_app(app) {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

pub fn read_pid_file(port: u16) -> Option<PidInfo> {
    let primary = pid_file_path_for(port);
    let mut paths = vec![primary];
    if port != proxy::DEFAULT_PORT {
        paths.push(legacy_pid_file_path());
    }
    paths.into_iter().find_map(|path| {
        let content = fs::read_to_string(path).ok()?;
        let info = serde_json::from_str::<PidInfo>(&content).ok()?;
        (info.port == port).then_some(info)
    })
}

pub fn write_pid_file(port: u16, admin_token: &str) -> Result<(), String> {
    validate_port(port)?;
    let path = pid_file_path_for(port);
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
        admin_token: Some(admin_token.to_string()),
    };
    let json = serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?;
    config::atomic_write(&path, json.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置代理 PID 文件权限失败: {e}"))?;
    }
    Ok(())
}

pub fn remove_pid_file(port: u16) {
    let _ = fs::remove_file(pid_file_path_for(port));
    if port != proxy::DEFAULT_PORT {
        // 只清理 legacy 文件中明确属于这个端口的记录，不能误删默认
        // 端口上另一实例的 PID 文件。
        let legacy = legacy_pid_file_path();
        let belongs_to_port = fs::read_to_string(&legacy)
            .ok()
            .and_then(|content| serde_json::from_str::<PidInfo>(&content).ok())
            .is_some_and(|info| info.port == port);
        if belongs_to_port {
            let _ = fs::remove_file(legacy);
        }
    }
}

fn remove_owned_pid_file(port: u16, pid: u32) {
    if read_pid_file(port).is_some_and(|info| info.pid == pid) {
        remove_pid_file(port);
    }
}

/// 判断 PID 文件对应的进程是否仍存在。
/// `None` 表示当前平台无法可靠判断，此时宁可保留 PID 文件，避免误删
/// 新代理实例或误把 PID 重用的其它进程当成自己的进程。
fn process_is_alive(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(false);
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let pid_text = pid.to_string();
        let output = String::from_utf8_lossy(&output.stdout);
        return Some(output.lines().any(|line| {
            let fields: Vec<_> = line
                .split(',')
                .map(|field| field.trim().trim_matches('"'))
                .collect();
            fields.get(1).is_some_and(|field| *field == pid_text)
        }));
    }
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .ok()?;
        return Some(status.success());
    }
    #[allow(unreachable_code)]
    None
}

// ---------------- IPC 客户端 ----------------

fn http_client(timeout_ms: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(timeout_ms))
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .unwrap_or_default()
}

const ADMIN_TOKEN_HEADER: &str = "x-z-switch-admin-token";

fn admin_token(port: u16) -> Option<String> {
    read_pid_file(port).and_then(|info| info.admin_token)
}

fn pid_matches_health(port: u16, health: &AdminHealthResponse) -> bool {
    health.port == port && read_pid_file(port).is_some_and(|info| info.pid == health.pid)
}

async fn verify_owned_proxy(port: u16) -> Result<AdminHealthResponse, String> {
    let health = check_health(port).await?;
    if !pid_matches_health(port, &health) {
        return Err(format!(
            "端口 {port} 不是当前 CLI 代理实例，拒绝执行控制操作"
        ));
    }
    Ok(health)
}

pub async fn check_health(port: u16) -> Result<AdminHealthResponse, String> {
    validate_port(port)?;
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
    validate_port(port)?;
    verify_owned_proxy(port).await?;
    let url = format!("http://127.0.0.1:{port}/_admin/status");
    let mut request = http_client(2000).get(&url);
    if let Some(token) = admin_token(port) {
        request = request.header(ADMIN_TOKEN_HEADER, token);
    }
    let resp = request
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
    validate_port(port)?;
    verify_owned_proxy(port).await?;
    let url = format!("http://127.0.0.1:{port}/_admin/switch");
    let payload = AdminSwitchRequest {
        app: app.to_string(),
        target,
    };
    let mut request = http_client(3000).post(&url).json(&payload);
    if let Some(token) = admin_token(port) {
        request = request.header(ADMIN_TOKEN_HEADER, token);
    }
    let resp = request
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
    validate_port(port)?;
    verify_owned_proxy(port).await?;
    let url = format!("http://127.0.0.1:{port}/_admin/shutdown");
    let mut request = http_client(2000).post(&url);
    if let Some(token) = admin_token(port) {
        request = request.header(ADMIN_TOKEN_HEADER, token);
    }
    let resp = request
        .send()
        .await
        .map_err(|e| format!("发送停止代理信令失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("停止接口响应异常: {}", resp.status()));
    }
    Ok(())
}

// ---------------- 守护进程启停逻辑 ----------------

pub async fn is_running(port: u16) -> bool {
    let Ok(health) = check_health(port).await else {
        return false;
    };
    // 仅匹配 PID 不足以证明端口属于 CLI：残留 PID 文件在 PID 重用后
    // 可能恰好指向其它本地服务。再验证带 token 的 status 接口，避免
    // 把外部进程误判为自有代理并覆盖其路由。
    pid_matches_health(port, &health) && get_status(port).await.is_ok()
}

/// 仅检查本地端口是否有进程接受连接，不要求它是 CLI 代理。
/// 用于 doctor 识别 GUI 的 8899 代理，避免把另一个实现误报为残留。
pub async fn is_port_open(port: u16) -> bool {
    validate_port(port).is_ok()
        && tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        .is_ok_and(|result| result.is_ok())
}

/// 返回当前应用是否正在被一个已监听、且不是当前 CLI 代理的本地端口接管。
///
/// GUI 默认使用 8899，而 CLI 默认使用 8999。共享 live 配置意味着同一个
/// 应用不能被两个进程同时控制；写入前必须把活动中的外部代理报告给调用方。
pub async fn active_foreign_proxy_port(app: &str, cli_port: u16) -> Option<u16> {
    let live_port = live::proxy_port(app)?;
    // 不能用 `live_port == DEFAULT_PORT` 判断归属。GUI、其它版本的 CLI
    // 或任意本地程序都可能监听同一个端口；必须通过健康接口和 PID 文件
    // 的进程身份双重确认。
    let cli_owned = live_port == cli_port && is_running(live_port).await;
    if cli_owned || !is_port_open(live_port).await {
        None
    } else {
        Some(live_port)
    }
}

pub async fn start_background(port: u16) -> Result<(), String> {
    start_background_owned(port).await.map(|_| ())
}

/// 启动代理并报告本次调用是否真正创建了 worker。
///
/// 这个结果用于事务回滚：两个终端并发启动时，后拿到生命周期锁的调用
/// 可能只是复用了前一个终端刚启动的 worker，不能把它误认为“本次创建”
/// 并在后续失败时停止它。
pub(crate) async fn start_background_owned(port: u16) -> Result<StartOutcome, String> {
    validate_port(port)?;
    // 启动检查、spawn、健康探测必须串行，否则两个终端可能同时通过
    // “端口尚未监听”的检查，各自拉起一个 worker，后启动者再误报端口冲突。
    let _lifecycle_lock = acquire_lifecycle_lock(port).await?;
    start_background_locked_owned(port).await
}

pub(crate) async fn start_background_locked(port: u16) -> Result<(), String> {
    start_background_locked_owned(port).await.map(|_| ())
}

async fn start_background_locked_owned(port: u16) -> Result<StartOutcome, String> {
    validate_port(port)?;
    if let Ok(health) = check_health(port).await {
        if !pid_matches_health(port, &health) || get_status(port).await.is_err() {
            return Err(format!(
                "端口 {port} 已被其它进程或不兼容的旧代理占用，请先检查并释放该端口"
            ));
        }
        return Ok(StartOutcome {
            pid: health.pid,
            started: false,
        });
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

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("拉起后台代理进程失败: {e}"))?;

    // 等待就绪
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if let Ok(Some(status)) = child.try_wait() {
            remove_owned_pid_file(port, child.id());
            return Err(format!("后台代理进程启动失败: {status}"));
        }
        if let Ok(health) = check_health(port).await {
            let ready = health.port == port
                && read_pid_file(port).is_some_and(|info| info.pid == health.pid);
            if ready {
                return Ok(StartOutcome {
                    pid: health.pid,
                    started: true,
                });
            }
        }
    }
    let child_pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    remove_owned_pid_file(port, child_pid);
    Err(format!("后台代理进程启动超时（5秒内未响应端口 {port}）"))
}

/// 只停止调用方明确创建的 worker。端口可能已经被同一 CLI 的新实例
/// 接管，因此不能仅凭端口执行回滚停机。
pub(crate) async fn stop_if_pid(port: u16, expected_pid: u32) -> Result<(), String> {
    validate_port(port)?;
    let _lifecycle_lock = acquire_lifecycle_lock(port).await?;
    let Some(info) = read_pid_file(port) else {
        return Ok(());
    };
    if info.pid != expected_pid {
        return Ok(());
    }
    stop_locked(port).await
}

pub(crate) async fn stop_locked(port: u16) -> Result<(), String> {
    validate_port(port)?;
    let health = match check_health(port).await {
        Ok(health) if health.port == port => health,
        Ok(health) => {
            return Err(format!(
                "端口 {port} 返回了其它代理端口 {} 的健康响应",
                health.port
            ));
        }
        Err(error) => {
            // 健康接口不可用不等于端口已经关闭。这里可能是 GUI、旧版代理
            // 或任意其它本地服务；没有 CLI PID 文件时也不能误报“已停止”。
            if is_port_open(port).await {
                return Err(format!(
                    "端口 {port} 仍被其它进程或不兼容的代理占用，无法确认归属，未执行停止: {error}"
                ));
            }
            // 端口已关闭时可以清理明确属于已退出进程的残留 PID；无法确认
            // 进程状态时保留文件，避免与刚启动的实例或 PID 重用发生竞态。
            if let Some(info) = read_pid_file(port) {
                match process_is_alive(info.pid) {
                    Some(false) => remove_owned_pid_file(port, info.pid),
                    Some(true) => {
                        return Err(format!(
                            "代理进程 {} 仍存在，但健康接口不可用: {error}",
                            info.pid
                        ));
                    }
                    None => {}
                }
            }
            return Ok(());
        }
    };

    if !pid_matches_health(port, &health) || get_status(port).await.is_err() {
        return Err(format!(
            "端口 {port} 不是当前 CLI 代理实例，未执行停止或强杀"
        ));
    }

    let target_pid = health.pid;
    send_shutdown(port).await?;

    // 等待退出
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !is_running(port).await {
            remove_owned_pid_file(port, target_pid);
            return Ok(());
        }
    }

    // 强杀兜底：在真正执行前再次确认健康接口、PID 文件和管理 token。
    // 只读 PID 文件不足以防止极端的 PID 重用竞态，不能据此误杀其它进程。
    let still_owned = match check_health(port).await {
        Ok(health) if pid_matches_health(port, &health) => get_status(port).await.is_ok(),
        _ => false,
    };
    if !still_owned {
        if is_port_open(port).await {
            return Err(format!(
                "代理优雅停机超时，但无法再次确认端口 {port} 的进程归属，未执行强杀"
            ));
        }
        remove_owned_pid_file(port, target_pid);
        return Ok(());
    }

    if let Some(info) = read_pid_file(port).filter(|info| info.pid == target_pid) {
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
        tokio::time::sleep(Duration::from_millis(250)).await;
        if is_running(port).await {
            return Err(format!("代理进程 {} 未能退出", info.pid));
        }
        remove_owned_pid_file(port, target_pid);
        return Ok(());
    }

    if is_running(port).await {
        return Err("代理未在优雅停机后退出，且缺少匹配的 PID 文件，未执行强杀".into());
    }
    remove_owned_pid_file(port, target_pid);
    Ok(())
}

/// 后台常驻工作进程入口 (由 `z-switch proxy worker --port <port>` 启动)
pub async fn run_worker(port: u16) -> Result<(), String> {
    validate_port(port)?;
    let root = store::load_checked()?;
    let runtime_cfg = ProxyRuntimeConfig::from_settings(&root.settings);

    let handle = ProxyHandle::default();
    // 注入已选中的供应商
    for &app in proxy::PROXY_APPS {
        if let Some(app_data) = root.apps.get(app) {
            if let Some(cur_id) = &app_data.current {
                if let Some(provider) = app_data.providers.get(cur_id) {
                    if !store::is_official_provider_for_app(app, provider)
                        && live::proxy_port(app) == Some(port)
                    {
                        if let Some(target) = proxy::target_from_provider(app, provider) {
                            if !proxy::is_self_proxy_target(&target.base_url, port) {
                                proxy::set_target(&handle.targets, app, target);
                            }
                        }
                    }
                }
            }
        }
    }

    let mut control = ProxyControl::new(handle);
    let admin_token = create_admin_token();
    control
        .start(port, runtime_cfg, admin_token.clone())
        .await?;

    if let Err(e) = write_pid_file(port, &admin_token) {
        control.stop();
        remove_owned_pid_file(port, std::process::id());
        return Err(format!("写入代理 PID 文件失败，已取消启动: {e}"));
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
    remove_owned_pid_file(port, std::process::id());
    Ok(())
}

fn create_admin_token() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, routing::get, Router};

    #[tokio::test]
    async fn stop_rejects_a_foreign_process_on_the_requested_port() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test listener");
        let port = listener.local_addr().expect("test address").port();
        let server = tokio::spawn(async move {
            let app =
                Router::new().route("/_admin/health", get(|| async { StatusCode::NOT_FOUND }));
            let _ = axum::serve(listener, app).await;
        });

        let error = stop_locked(port)
            .await
            .expect_err("foreign process must not be reported as stopped");
        assert!(error.contains("无法确认归属"), "unexpected error: {error}");
        assert!(is_port_open(port).await);

        server.abort();
    }

    #[test]
    fn validate_port_rejects_zero_only() {
        assert!(validate_port(0).is_err());
        assert!(validate_port(1).is_ok());
        assert!(validate_port(u16::MAX).is_ok());
    }
}
