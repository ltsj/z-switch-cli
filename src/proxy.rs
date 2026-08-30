//! 本地轻量高性能反向代理服务与 Admin 控制面。
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::{Json, Response},
    routing::{any, get, post},
    Router,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::proxy_log;

pub const DEFAULT_PORT: u16 = 8999;
pub const PROXY_APPS: &[&str] = &["claude", "codex", "grok"];
pub const PLACEHOLDER_KEY: &str = "z-switch-proxy";

/// 判断是否为 CLI 写入 live 配置时使用的占位凭据。
///
/// 供应商数据可能来自手工编辑、GUI 或旧版导入，前后空白不能改变它
/// 的语义；所有持久化/导入校验都应使用这个函数，避免把占位值当真实
/// API Key 保存或转发。
pub fn is_placeholder_key(value: &str) -> bool {
    value.trim() == PLACEHOLDER_KEY
}

fn is_placeholder_auth_value(value: &str) -> bool {
    let value = value.trim();
    if is_placeholder_key(value) {
        return true;
    }
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next().unwrap_or_default();
    scheme.eq_ignore_ascii_case("bearer") && parts.next().is_some_and(is_placeholder_key)
}

#[derive(Clone, Debug)]
pub struct ProxyRuntimeConfig {
    pub connect_timeout: Duration,
    pub streaming_first_byte_timeout: Duration,
    pub streaming_idle_timeout: Duration,
    pub non_streaming_timeout: Duration,
    pub request_body_limit_bytes: usize,
    pub pool_max_idle_per_host: usize,
    pub tcp_keepalive: Duration,
    pub error_log_enabled: bool,
    pub error_log_max_mb: u64,
}

impl Default for ProxyRuntimeConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            streaming_first_byte_timeout: Duration::from_secs(60),
            streaming_idle_timeout: Duration::from_secs(120),
            non_streaming_timeout: Duration::from_secs(600),
            request_body_limit_bytes: 64 * 1024 * 1024,
            pool_max_idle_per_host: 10,
            tcp_keepalive: Duration::from_secs(60),
            error_log_enabled: true,
            error_log_max_mb: 5,
        }
    }
}

impl ProxyRuntimeConfig {
    pub fn from_settings(settings: &serde_json::Value) -> Self {
        let defaults = Self::default();
        let reliability = settings.get("reliability");
        let number = |key: &str, fallback: u64, min: u64, max: u64| {
            reliability
                .and_then(|value| value.get(key))
                .and_then(|value| value.as_u64())
                .unwrap_or(fallback)
                .clamp(min, max)
        };
        let request_body_mb = number(
            "requestBodyLimitMb",
            (defaults.request_body_limit_bytes / 1024 / 1024) as u64,
            1,
            256,
        );
        Self {
            connect_timeout: Duration::from_secs(number(
                "connectTimeoutSeconds",
                defaults.connect_timeout.as_secs(),
                1,
                120,
            )),
            streaming_first_byte_timeout: Duration::from_secs(number(
                "streamingFirstByteTimeoutSeconds",
                defaults.streaming_first_byte_timeout.as_secs(),
                5,
                300,
            )),
            streaming_idle_timeout: Duration::from_secs(number(
                "streamingIdleTimeoutSeconds",
                defaults.streaming_idle_timeout.as_secs(),
                10,
                900,
            )),
            non_streaming_timeout: Duration::from_secs(number(
                "nonStreamingTimeoutSeconds",
                defaults.non_streaming_timeout.as_secs(),
                30,
                3600,
            )),
            request_body_limit_bytes: request_body_mb as usize * 1024 * 1024,
            pool_max_idle_per_host: number(
                "poolMaxIdlePerHost",
                defaults.pool_max_idle_per_host as u64,
                1,
                100,
            ) as usize,
            tcp_keepalive: Duration::from_secs(number(
                "tcpKeepaliveSeconds",
                defaults.tcp_keepalive.as_secs(),
                10,
                600,
            )),
            error_log_enabled: reliability
                .and_then(|value| value.get("proxyErrorLogEnabled"))
                .and_then(|value| value.as_bool())
                .unwrap_or(defaults.error_log_enabled),
            error_log_max_mb: number("proxyErrorLogMaxMb", defaults.error_log_max_mb, 1, 100),
        }
    }
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct AppTarget {
    pub base_url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Default)]
pub struct ProxyTargets {
    pub map: HashMap<String, AppTarget>,
}

pub type SharedTargets = Arc<RwLock<ProxyTargets>>;

#[derive(Clone)]
pub struct AppCounters {
    pub in_flight: Arc<AtomicU32>,
    pub total: Arc<AtomicU64>,
    pub last_activity_ms: Arc<AtomicU64>,
}

impl Default for AppCounters {
    fn default() -> Self {
        Self {
            in_flight: Arc::new(AtomicU32::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            last_activity_ms: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl AppCounters {
    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    pub fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms.load(Ordering::Relaxed)
    }
    fn reset(&self) {
        self.in_flight.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
        self.last_activity_ms.store(0, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct ProxyHandle {
    pub targets: SharedTargets,
    pub running: Arc<AtomicBool>,
    pub port: Arc<std::sync::atomic::AtomicU16>,
    pub routed: Arc<RwLock<HashSet<String>>>,
    pub counters: HashMap<String, AppCounters>,
}

impl Default for ProxyHandle {
    fn default() -> Self {
        let mut counters = HashMap::new();
        for app in PROXY_APPS {
            counters.insert((*app).to_string(), AppCounters::default());
        }
        Self {
            targets: Arc::new(RwLock::new(ProxyTargets::default())),
            running: Arc::new(AtomicBool::new(false)),
            port: Arc::new(std::sync::atomic::AtomicU16::new(DEFAULT_PORT)),
            routed: Arc::new(RwLock::new(HashSet::new())),
            counters,
        }
    }
}

#[allow(dead_code)]
impl ProxyHandle {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn current_port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }
    pub fn is_routed(&self, app: &str) -> bool {
        self.routed.read().map(|s| s.contains(app)).unwrap_or(false)
    }
    pub fn set_routed(&self, app: &str, on: bool) {
        if let Ok(mut s) = self.routed.write() {
            if on {
                s.insert(app.to_string());
            } else {
                s.remove(app);
            }
        }
    }
    pub fn routed_count(&self) -> usize {
        self.routed.read().map(|s| s.len()).unwrap_or(0)
    }
    pub fn counters(&self, app: &str) -> Option<&AppCounters> {
        self.counters.get(app)
    }
    pub fn reset_counters(&self) {
        for counter in self.counters.values() {
            counter.reset();
        }
    }
}

struct Runtime {
    client: reqwest::Client,
    targets: SharedTargets,
    config: ProxyRuntimeConfig,
    error_log_lock: tokio::sync::Mutex<()>,
    counters: HashMap<String, AppCounters>,
    port: u16,
    shutdown_sender: mpsc::Sender<()>,
    admin_token: String,
}

struct InFlightGuard(Option<Arc<AtomicU32>>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Some(counter) = &self.0 {
            // reset_counters() may run while an older response is still being
            // streamed.  A plain fetch_sub would wrap 0 to u32::MAX in that
            // case and make the status output invalid until the next restart.
            let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_sub(1))
            });
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------- Admin DTO ----------------

#[derive(Serialize, Deserialize)]
pub struct AdminHealthResponse {
    pub status: String,
    pub port: u16,
    pub pid: u32,
}

#[derive(Serialize, Deserialize)]
pub struct AdminStatusResponse {
    pub running: bool,
    pub port: u16,
    pub pid: u32,
    pub routed_apps: Vec<String>,
    pub targets: HashMap<String, String>, // app -> base_url
    pub counters: HashMap<String, AppCounterDto>,
}

#[derive(Serialize, Deserialize)]
pub struct AppCounterDto {
    pub in_flight: u32,
    pub total: u64,
    pub last_activity_ms: u64,
}

#[derive(Serialize, Deserialize)]
pub struct AdminSwitchRequest {
    pub app: String,
    pub target: Option<AppTarget>,
}

#[derive(Serialize, Deserialize)]
pub struct AdminSwitchResponse {
    pub ok: bool,
    pub app: String,
    pub base_url: Option<String>,
}

// ---------------- Proxy Control ----------------

pub struct ProxyControl {
    pub handle: ProxyHandle,
    shutdown: Option<oneshot::Sender<()>>,
}

impl ProxyControl {
    pub fn new(handle: ProxyHandle) -> Self {
        Self {
            handle,
            shutdown: None,
        }
    }

    pub async fn start(
        &mut self,
        port: u16,
        config: ProxyRuntimeConfig,
        admin_token: String,
    ) -> Result<(), String> {
        if port == 0 {
            return Err("代理端口必须在 1 到 65535 之间".into());
        }
        self.stop();
        let addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("代理端口 {port} 绑定失败 (可能被占用): {e}"))?;

        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .pool_max_idle_per_host(config.pool_max_idle_per_host)
            .tcp_keepalive(config.tcp_keepalive)
            .build()
            .map_err(|e| format!("构建代理 HTTP 客户端失败: {e}"))?;

        self.handle.reset_counters();
        let (admin_shutdown_tx, mut admin_shutdown_rx) = mpsc::channel::<()>(1);
        let runtime = Arc::new(Runtime {
            client,
            targets: self.handle.targets.clone(),
            config,
            error_log_lock: tokio::sync::Mutex::new(()),
            counters: self.handle.counters.clone(),
            port,
            shutdown_sender: admin_shutdown_tx,
            admin_token,
        });

        let app = Router::new()
            // Admin 控制面端点 (仅限本地管理)
            .route("/_admin/health", get(admin_health))
            .route("/_admin/status", get(admin_status))
            .route("/_admin/switch", post(admin_switch))
            .route("/_admin/shutdown", post(admin_shutdown))
            // Data 面代理转发路由
            .route("/{app}/{*rest}", any(forward))
            .route("/{app}", any(forward))
            .with_state(runtime);

        let (tx, rx) = oneshot::channel::<()>();
        self.shutdown = Some(tx);
        self.handle.port.store(port, Ordering::SeqCst);
        self.handle.running.store(true, Ordering::SeqCst);

        let running = self.handle.running.clone();
        tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                tokio::select! {
                    _ = rx => {},
                    _ = admin_shutdown_rx.recv() => {},
                }
            });
            if let Err(e) = server.await {
                eprintln!("[z-switch] 代理服务退出: {e}");
            }
            running.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.handle.running.store(false, Ordering::SeqCst);
    }
}

// ---------------- Admin Handlers ----------------

async fn admin_health(State(rt): State<Arc<Runtime>>) -> Json<AdminHealthResponse> {
    Json(AdminHealthResponse {
        status: "ok".into(),
        port: rt.port,
        pid: std::process::id(),
    })
}

fn authorized(headers: &HeaderMap, rt: &Runtime) -> bool {
    headers
        .get("x-z-switch-admin-token")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !rt.admin_token.is_empty() && value == rt.admin_token)
}

async fn admin_status(
    State(rt): State<Arc<Runtime>>,
    headers: HeaderMap,
) -> Result<Json<AdminStatusResponse>, StatusCode> {
    if !authorized(&headers, &rt) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut targets = HashMap::new();
    let mut routed_apps = Vec::new();
    if let Ok(guard) = rt.targets.read() {
        for (app, target) in &guard.map {
            targets.insert(app.clone(), proxy_log::sanitize_url(&target.base_url));
            routed_apps.push(app.clone());
        }
    }
    let mut counters = HashMap::new();
    for (app, c) in &rt.counters {
        counters.insert(
            app.clone(),
            AppCounterDto {
                in_flight: c.in_flight(),
                total: c.total(),
                last_activity_ms: c.last_activity_ms(),
            },
        );
    }
    Ok(Json(AdminStatusResponse {
        running: true,
        port: rt.port,
        pid: std::process::id(),
        routed_apps,
        targets,
        counters,
    }))
}

async fn admin_switch(
    State(rt): State<Arc<Runtime>>,
    headers: HeaderMap,
    Json(payload): Json<AdminSwitchRequest>,
) -> Result<Json<AdminSwitchResponse>, StatusCode> {
    if !authorized(&headers, &rt) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let app = payload.app.to_lowercase();
    if !PROXY_APPS.contains(&app.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(target) = payload.target.as_ref() {
        let parsed =
            reqwest::Url::parse(target.base_url.trim()).map_err(|_| StatusCode::BAD_REQUEST)?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if is_self_proxy_target(&target.base_url, rt.port) {
            return Err(StatusCode::BAD_REQUEST);
        }
        for (name, value) in &target.headers {
            reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            reqwest::header::HeaderValue::from_str(value).map_err(|_| StatusCode::BAD_REQUEST)?;
            if is_placeholder_auth_value(value) {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }
    let base_url = if let Some(target) = payload.target {
        let b = target.base_url.clone();
        set_target(&rt.targets, &app, target);
        Some(b)
    } else {
        clear_target(&rt.targets, &app);
        None
    };
    Ok(Json(AdminSwitchResponse {
        ok: true,
        app,
        base_url,
    }))
}

async fn admin_shutdown(
    State(rt): State<Arc<Runtime>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !authorized(&headers, &rt) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let sender = rt.shutdown_sender.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = sender.send(()).await;
    });
    Ok(Json(serde_json::json!({
        "ok": true,
        "message": "z-switch 后台代理正在安全退出"
    })))
}

// ---------------- Forward Handler ----------------

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

fn hop_by_hop_headers<I>(connection_values: I) -> HashSet<String>
where
    I: IntoIterator<Item = String>,
{
    let mut names = HOP_BY_HOP
        .iter()
        .map(|name| (*name).to_string())
        .collect::<HashSet<_>>();
    for value in connection_values {
        for name in value.split(',') {
            let name = name.trim().to_ascii_lowercase();
            if !name.is_empty() {
                names.insert(name);
            }
        }
    }
    names
}

const AUTH_HEADERS: &[&str] = &["authorization", "x-api-key", "api-key"];
const ERROR_BODY_CAPTURE_BYTES: usize = 64 * 1024;

#[derive(serde::Deserialize, Default)]
struct StreamHint {
    #[serde(default)]
    stream: bool,
}

fn target_secrets(target: &AppTarget) -> Vec<String> {
    target
        .headers
        .iter()
        .map(|(_, value)| value.clone())
        .collect()
}

/// Build the upstream request headers while preserving valid repeated client
/// headers. Authentication supplied by the selected target intentionally
/// replaces any client value, so a target header name is removed once before
/// its one or more configured values are appended.
fn forward_headers(
    headers: &HeaderMap,
    target_headers: &[(String, String)],
) -> reqwest::header::HeaderMap {
    let request_hop_by_hop = hop_by_hop_headers(
        headers
            .get_all("connection")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string),
    );
    let mut forwarded = reqwest::header::HeaderMap::new();
    for (name, value) in headers.iter() {
        let lower_name = name.as_str().to_ascii_lowercase();
        if request_hop_by_hop.contains(lower_name.as_str())
            || AUTH_HEADERS.contains(&lower_name.as_str())
            || lower_name == "x-z-switch-admin-token"
        {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            forwarded.append(name, value);
        }
    }

    let target_hop_by_hop = hop_by_hop_headers(
        target_headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
            .map(|(_, value)| value.clone()),
    );
    let mut target_names = HashSet::new();
    for (raw_name, raw_value) in target_headers {
        let lower_name = raw_name.to_ascii_lowercase();
        if target_hop_by_hop.contains(&lower_name) || lower_name == "x-z-switch-admin-token" {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(raw_name.as_bytes()),
            reqwest::header::HeaderValue::from_str(raw_value),
        ) {
            if target_names.insert(name.clone()) {
                forwarded.remove(&name);
            }
            forwarded.append(name, value);
        }
    }
    forwarded
}

fn safe_error_detail(raw: &str, url: &str, secrets: &[String]) -> String {
    let safe_url = proxy_log::sanitize_url(url);
    proxy_log::redact_and_truncate(&raw.replace(url, &safe_url), secrets)
}

async fn write_proxy_error(
    rt: &Arc<Runtime>,
    app: &str,
    status: Option<u16>,
    url: &str,
    phase: &str,
    detail: &str,
    secrets: &[String],
) {
    if !rt.config.error_log_enabled {
        return;
    }
    let safe_url = proxy_log::sanitize_url(url);
    let detail = safe_error_detail(detail, url, secrets);
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let entry = proxy_log::ProxyErrorEntry {
        timestamp_ms,
        app,
        status,
        url: &safe_url,
        phase,
        detail: &detail,
    };
    let _guard = rt.error_log_lock.lock().await;
    if let Err(error) = proxy_log::append(&entry, rt.config.error_log_max_mb) {
        eprintln!("[z-switch] 写入路由错误日志失败：{error}");
    }
}

async fn forward(
    State(rt): State<Arc<Runtime>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, (StatusCode, String)> {
    let path = uri.path();
    let trimmed = path.trim_start_matches('/');
    let (app, rest) = match trimmed.split_once('/') {
        Some((a, r)) => (a.to_string(), format!("/{r}")),
        None => (trimmed.to_string(), String::new()),
    };

    let app_counters = rt.counters.get(&app).cloned();
    if let Some(counter) = &app_counters {
        counter.total.fetch_add(1, Ordering::Relaxed);
        counter.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        counter.in_flight.fetch_add(1, Ordering::Relaxed);
    }
    let in_flight_guard = InFlightGuard(app_counters.as_ref().map(|c| c.in_flight.clone()));

    let target = {
        let guard = rt.targets.read().unwrap();
        guard.map.get(&app).cloned()
    };
    let target = target.ok_or_else(|| {
        (
            StatusCode::BAD_GATEWAY,
            format!("代理未配置目标：{app}（请在 z-switch 里选择一个供应商）"),
        )
    })?;
    if target.base_url.trim().is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("代理目标 {app} 的 base_url 为空"),
        ));
    }

    let url = build_target_url(&target.base_url, &rest, uri.query());

    // Provider 配置可能来自外部导入；如果它误指向当前代理端口，直接转发
    // 会递归命中自己，最终耗尽连接或请求栈。其它本机端口仍允许作为上游。
    if is_self_proxy_target(&url, rt.port) {
        let detail = format!("代理目标指向自身监听地址，已阻止递归转发：{}", rt.port);
        let secrets = target_secrets(&target);
        write_proxy_error(&rt, &app, None, &url, "self_proxy_loop", &detail, &secrets).await;
        return Err((StatusCode::LOOP_DETECTED, detail));
    }

    let body_limit = rt.config.request_body_limit_bytes;
    if headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > body_limit)
    {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "请求体超过本地路由限制（最大 {}MB）",
                body_limit / 1024 / 1024
            ),
        ));
    }
    let body_bytes = axum::body::to_bytes(body, body_limit)
        .await
        .map_err(|error| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "读取请求体失败或超过本地路由限制（最大 {}MB）：{error}",
                    body_limit / 1024 / 1024
                ),
            )
        })?;

    let is_streaming = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
        || serde_json::from_slice::<StreamHint>(&body_bytes)
            .map(|hint| hint.stream)
            .unwrap_or(false);

    let fwd = forward_headers(&headers, &target.headers);

    let mut request = rt
        .client
        .request(method, &url)
        .headers(fwd)
        .body(body_bytes);
    if !is_streaming {
        request = request.timeout(rt.config.non_streaming_timeout);
    }
    let send = request.send();
    let send_result = if is_streaming {
        match tokio::time::timeout(rt.config.streaming_first_byte_timeout, send).await {
            Ok(result) => result,
            Err(_) => {
                let detail = format!(
                    "等待上游响应头超时（{}秒）",
                    rt.config.streaming_first_byte_timeout.as_secs()
                );
                let secrets = target_secrets(&target);
                write_proxy_error(
                    &rt,
                    &app,
                    None,
                    &url,
                    "response_header_timeout",
                    &detail,
                    &secrets,
                )
                .await;
                return Err((StatusCode::GATEWAY_TIMEOUT, detail));
            }
        }
    } else {
        send.await
    };
    let secrets = target_secrets(&target);
    let upstream = match send_result {
        Ok(response) => response,
        Err(error) => {
            let detail =
                safe_error_detail(&format!("连接或发送上游请求失败：{error}"), &url, &secrets);
            write_proxy_error(&rt, &app, None, &url, "request", &detail, &secrets).await;
            return Err((StatusCode::BAD_GATEWAY, detail));
        }
    };

    let status = upstream.status();
    let mut builder = Response::builder().status(status.as_u16());
    let response_hop_by_hop = hop_by_hop_headers(
        upstream
            .headers()
            .get_all("connection")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string),
    );
    for (name, value) in upstream.headers().iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if response_hop_by_hop.contains(lname.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(n, v);
        }
    }

    let status_code = status.as_u16();
    let log_upstream_error = !status.is_success();
    let stream_rt = rt.clone();
    let stream_app = app.clone();
    let stream_url = url.clone();
    let stream_secrets = secrets.clone();
    let first_timeout = rt.config.streaming_first_byte_timeout;
    let idle_timeout = rt.config.streaming_idle_timeout;
    let non_streaming_timeout = rt.config.non_streaming_timeout;
    let upstream_stream = Box::pin(upstream.bytes_stream());

    let stream = futures_util::stream::unfold(
        (
            upstream_stream,
            true,
            false,
            Vec::<u8>::new(),
            in_flight_guard,
        ),
        move |(mut upstream_stream, first_chunk, finished, mut capture, guard)| {
            let rt = stream_rt.clone();
            let app = stream_app.clone();
            let url = stream_url.clone();
            let secrets = stream_secrets.clone();
            async move {
                if finished {
                    return None;
                }
                let timeout = if is_streaming {
                    if first_chunk {
                        first_timeout
                    } else {
                        idle_timeout
                    }
                } else {
                    non_streaming_timeout
                };
                match tokio::time::timeout(timeout, upstream_stream.next()).await {
                    Ok(Some(Ok(bytes))) => {
                        if log_upstream_error && capture.len() < ERROR_BODY_CAPTURE_BYTES {
                            let remaining = ERROR_BODY_CAPTURE_BYTES - capture.len();
                            capture.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
                        }
                        Some((Ok(bytes), (upstream_stream, false, false, capture, guard)))
                    }
                    Ok(Some(Err(error))) => {
                        let detail = safe_error_detail(
                            &format!("读取上游响应流失败：{error}"),
                            &url,
                            &secrets,
                        );
                        write_proxy_error(
                            &rt,
                            &app,
                            Some(status_code),
                            &url,
                            "response_stream",
                            &detail,
                            &secrets,
                        )
                        .await;
                        Some((
                            Err(std::io::Error::other(detail)),
                            (upstream_stream, false, true, capture, guard),
                        ))
                    }
                    Ok(None) => {
                        if log_upstream_error {
                            let detail = if capture.is_empty() {
                                "上游返回错误状态，但响应体为空".to_string()
                            } else {
                                String::from_utf8_lossy(&capture).into_owned()
                            };
                            write_proxy_error(
                                &rt,
                                &app,
                                Some(status_code),
                                &url,
                                "upstream",
                                &detail,
                                &secrets,
                            )
                            .await;
                        }
                        None
                    }
                    Err(_) => {
                        let phase = if first_chunk {
                            "first_byte_timeout"
                        } else {
                            "stream_idle_timeout"
                        };
                        let detail = format!("上游响应等待超时（{}秒）", timeout.as_secs());
                        write_proxy_error(
                            &rt,
                            &app,
                            Some(status_code),
                            &url,
                            phase,
                            &detail,
                            &secrets,
                        )
                        .await;
                        Some((
                            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, detail)),
                            (upstream_stream, false, true, capture, guard),
                        ))
                    }
                }
            }
        },
    );
    let resp = builder.body(Body::from_stream(stream)).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("构建响应失败：{e}"),
        )
    })?;
    Ok(resp)
}

pub fn target_from_provider(app: &str, provider: &crate::store::Provider) -> Option<AppTarget> {
    let target = match app {
        "claude" => {
            let env = provider.settings_config.get("env")?.as_object()?;
            let base = env.get("ANTHROPIC_BASE_URL")?.as_str()?.trim().to_string();
            if base.is_empty() || validate_base_url(&base).is_err() {
                return None;
            }
            let key_field = provider
                .meta
                .get("apiKeyField")
                .and_then(|v| v.as_str())
                .unwrap_or("ANTHROPIC_AUTH_TOKEN");
            let preferred_field = if key_field == "ANTHROPIC_API_KEY" {
                "ANTHROPIC_API_KEY"
            } else {
                "ANTHROPIC_AUTH_TOKEN"
            };
            let fallback_field = if preferred_field == "ANTHROPIC_API_KEY" {
                "ANTHROPIC_AUTH_TOKEN"
            } else {
                "ANTHROPIC_API_KEY"
            };
            let (key_field, key) = env
                .get(preferred_field)
                .and_then(|v| v.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(|value| (preferred_field, value.trim()))
                .or_else(|| {
                    env.get(fallback_field)
                        .and_then(|v| v.as_str())
                        .filter(|value| !value.trim().is_empty())
                        .map(|value| (fallback_field, value.trim()))
                })
                .unwrap_or((preferred_field, ""));
            let mut headers = Vec::new();
            if !key.is_empty() {
                if key_field == "ANTHROPIC_API_KEY" {
                    headers.push(("x-api-key".to_string(), key.to_string()));
                } else {
                    headers.push(("authorization".to_string(), format!("Bearer {key}")));
                }
            }
            Some(AppTarget {
                base_url: base,
                headers,
            })
        }
        "codex" => {
            let cfg = provider.settings_config.get("config")?.as_str()?;
            let base = crate::store::extract_codex_provider_string(cfg, "base_url")?
                .trim()
                .to_string();
            if base.is_empty() || validate_base_url(&base).is_err() {
                return None;
            }
            let key = provider.extract_api_key("codex").unwrap_or_default();
            let mut headers = Vec::new();
            if !key.trim().is_empty() {
                headers.push((
                    "authorization".to_string(),
                    format!("Bearer {}", key.trim()),
                ));
            }
            Some(AppTarget {
                base_url: base,
                headers,
            })
        }
        "grok" => {
            let cfg = provider.settings_config.get("config")?.as_str()?;
            let base = crate::store::extract_grok_endpoint_string(cfg, "models_base_url")
                .or_else(|| crate::store::extract_grok_endpoint_string(cfg, "base_url"))?
                .trim()
                .to_string();
            if base.is_empty() || validate_base_url(&base).is_err() {
                return None;
            }
            // Keep proxy target extraction in sync with Provider::extract_api_key:
            // an empty preferred auth field must not hide a valid fallback field
            // or a credential stored in the TOML config.
            let key = provider.extract_api_key("grok").unwrap_or_default();
            let key = key.trim();
            let mut headers = Vec::new();
            if !key.is_empty() {
                headers.push(("authorization".to_string(), format!("Bearer {key}")));
            }
            Some(AppTarget {
                base_url: base,
                headers,
            })
        }
        _ => None,
    }?;

    // providers.json 可由 GUI、旧版本或用户手工编辑；如果其中残留了
    // 代理写入 live 配置时使用的占位凭据，后台 worker 不能把它当成真实
    // Key 发往上游。服务层会拒绝新的脏配置，这个运行期检查负责兜住
    // 已经落盘的历史损坏数据。
    let has_placeholder = target
        .headers
        .iter()
        .any(|(_, value)| is_placeholder_auth_value(value));
    (!has_placeholder).then_some(target)
}

pub fn set_target(targets: &SharedTargets, app: &str, target: AppTarget) {
    if let Ok(mut g) = targets.write() {
        g.map.insert(app.to_string(), target);
    }
}

pub fn clear_target(targets: &SharedTargets, app: &str) {
    if let Ok(mut guard) = targets.write() {
        guard.map.remove(app);
    }
}

pub fn local_base(port: u16, app: &str) -> String {
    format!("http://127.0.0.1:{port}/{app}")
}

pub fn validate_base_url(base_url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(base_url.trim())
        .map_err(|_| "Base URL 必须是合法的 http(s) 地址".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("Base URL 必须是合法的 http(s) 地址".into());
    }
    Ok(())
}

#[allow(dead_code)]
fn normalized_toml_section(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("[[") || !trimmed.starts_with('[') {
        return None;
    }

    // TOML 允许表头后跟行内注释，例如 `[model_providers.custom] # relay`。
    // 仅用 `ends_with(']')` 会漏掉该表头，随后可能把下一个同名键误判为
    // 上一个 section 的内容。扫描引号后再找真正的结束括号，兼容带 `]`
    // 的 quoted key。
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in trimmed.char_indices().skip(1) {
        if let Some(current_quote) = quote {
            if current_quote == '"' && escaped {
                escaped = false;
            } else if current_quote == '"' && ch == '\\' {
                escaped = true;
            } else if ch == current_quote {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch == ']' {
            let trailing = trimmed[index + ch.len_utf8()..].trim();
            if !trailing.is_empty() && !trailing.starts_with('#') {
                return None;
            }
            return Some(trimmed[1..index].replace(['"', '\''], ""));
        }
    }
    None
}

fn rewrite_toml_keys_in_sections(
    config_text: &str,
    target_sections: &[&str],
    replacements: &[(&str, &str)],
) -> String {
    let normalized_targets: Vec<String> = target_sections
        .iter()
        .map(|section| section.replace(['"', '\''], ""))
        .collect();
    let mut current_section = None;
    config_text
        .lines()
        .map(|line| {
            if crate::store::is_toml_array_section(line) {
                // Array-of-tables must not inherit the preceding ordinary
                // section. Keep its fields untouched because this helper
                // only rewrites ordinary table sections.
                current_section = None;
                return line.to_string();
            }
            if let Some(section) = crate::store::normalized_toml_section(line) {
                current_section = Some(section);
                return line.to_string();
            }
            let Some((raw_key, _)) = line.trim().split_once('=') else {
                return line.to_string();
            };
            let key = raw_key.trim();
            let normalized_key = key.trim_matches(['"', '\'']);
            let is_target_section = current_section.as_deref().is_some_and(|section| {
                normalized_targets.iter().any(|target| {
                    section == target || (target == "model" && section.starts_with("model."))
                })
            });
            let Some((_, value)) = replacements
                .iter()
                .find(|(candidate, _)| is_target_section && normalized_key == *candidate)
            else {
                return line.to_string();
            };
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            format!("{indent}{key} = {}", crate::store::quote_toml_string(value))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 只重写指定 TOML section 中的直接键，避免把 MCP 或其它 provider 的同名
/// `base_url` 一起替换成当前 CLI 代理地址。
fn rewrite_toml_keys_in_section(
    config_text: &str,
    target_section: &str,
    replacements: &[(&str, &str)],
) -> String {
    let target_section = target_section.replace(['"', '\''], "");
    let root_section = target_section.is_empty();
    let dotted_prefix = (!target_section.is_empty()).then(|| format!("{target_section}."));
    let mut current_section = None;
    let mut in_array_table = false;
    config_text
        .lines()
        .map(|line| {
            if crate::store::is_toml_array_section(line) {
                current_section = None;
                in_array_table = true;
                return line.to_string();
            }
            if let Some(section) = crate::store::normalized_toml_section(line) {
                current_section = Some(section);
                in_array_table = false;
                return line.to_string();
            }
            let Some((raw_key, _)) = line.trim().split_once('=') else {
                return line.to_string();
            };
            let key = raw_key.trim();
            let normalized_key = key.trim_matches(['"', '\'']);
            let is_target_section = if root_section {
                current_section.is_none() && !in_array_table
            } else {
                current_section.as_deref() == Some(target_section.as_str())
            };
            let Some((_, value)) = replacements.iter().find(|(candidate, _)| {
                (is_target_section && normalized_key == *candidate)
                    || dotted_prefix.as_deref().is_some_and(|prefix| {
                        normalized_key.strip_prefix(prefix) == Some(*candidate)
                    })
            }) else {
                return line.to_string();
            };
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            format!("{indent}{key} = {}", crate::store::quote_toml_string(value))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rewrite_codex_base_url(config_text: &str, local: &str) -> String {
    if let Some(provider) = crate::store::extract_codex_provider_id(config_text)
        .filter(|value| !value.trim().is_empty())
    {
        let section = format!("model_providers.{provider}");
        rewrite_toml_keys_in_section(config_text, &section, &[("base_url", local)])
    } else {
        // Older Codex configs may keep base_url at the document root instead
        // of under model_providers.<id>. target_from_provider accepts that
        // shape, so proxy mode must rewrite it as well.
        rewrite_toml_keys_in_section(config_text, "", &[("base_url", local)])
    }
}

pub fn proxied_provider(
    app: &str,
    provider: &crate::store::Provider,
    port: u16,
) -> crate::store::Provider {
    if crate::store::is_official_provider_for_app(app, provider) {
        return provider.clone();
    }
    let mut p = provider.clone();
    let local = local_base(port, app);
    match app {
        "claude" => {
            if let Some(env) = p
                .settings_config
                .get_mut("env")
                .and_then(|v| v.as_object_mut())
            {
                env.insert(
                    "ANTHROPIC_BASE_URL".into(),
                    serde_json::Value::String(local),
                );
                for key_field in ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"] {
                    env.insert(
                        key_field.to_string(),
                        serde_json::Value::String(PLACEHOLDER_KEY.to_string()),
                    );
                }
            }
        }
        "codex" => {
            if let Some(cfg) = p.settings_config.get("config").and_then(|v| v.as_str()) {
                let rewritten = rewrite_codex_base_url(cfg, &local);
                if let Some(o) = p.settings_config.as_object_mut() {
                    o.insert("config".into(), serde_json::Value::String(rewritten));
                }
            }
            if let Some(auth) = p
                .settings_config
                .get_mut("auth")
                .and_then(|v| v.as_object_mut())
            {
                auth.insert(
                    "OPENAI_API_KEY".into(),
                    serde_json::Value::String(PLACEHOLDER_KEY.to_string()),
                );
            }
        }
        "grok" => {
            if let Some(cfg) = p.settings_config.get("config").and_then(|v| v.as_str()) {
                let rewritten = rewrite_toml_keys_in_section(
                    cfg,
                    "endpoints",
                    &[("models_base_url", &local), ("base_url", &local)],
                );
                let rewritten = rewrite_toml_keys_in_section(
                    &rewritten,
                    "",
                    &[("models_base_url", &local), ("base_url", &local)],
                );
                let rewritten = rewrite_toml_keys_in_section(
                    &rewritten,
                    "",
                    &[
                        ("api_key", PLACEHOLDER_KEY),
                        ("grok_api_key", PLACEHOLDER_KEY),
                        ("xai_api_key", PLACEHOLDER_KEY),
                    ],
                );
                let rewritten = rewrite_toml_keys_in_sections(
                    &rewritten,
                    &["endpoints", "model"],
                    &[
                        ("api_key", PLACEHOLDER_KEY),
                        ("grok_api_key", PLACEHOLDER_KEY),
                        ("xai_api_key", PLACEHOLDER_KEY),
                    ],
                );
                if let Some(o) = p.settings_config.as_object_mut() {
                    o.insert("config".into(), serde_json::Value::String(rewritten));
                }
            }
            if let Some(auth) = p
                .settings_config
                .get_mut("auth")
                .and_then(|v| v.as_object_mut())
            {
                auth.insert(
                    "GROK_API_KEY".into(),
                    serde_json::Value::String(PLACEHOLDER_KEY.to_string()),
                );
            }
        }
        _ => {}
    }
    p
}

pub fn build_target_url(base_url: &str, rest: &str, query: Option<&str>) -> String {
    let base_without_fragment = base_url.trim().split('#').next().unwrap_or(base_url);
    let (base_path, base_query) = base_without_fragment
        .split_once('?')
        .map_or((base_without_fragment, None), |(path, query)| {
            (path, Some(query))
        });
    let mut base = base_path.trim_end_matches('/');
    if base.ends_with("/v1") && (rest.starts_with("/v1/") || rest == "/v1") {
        base = base.strip_suffix("/v1").unwrap_or(base);
    }
    let mut url = format!("{base}{rest}");
    let request_query = query.filter(|value| !value.is_empty());
    match (base_query.filter(|value| !value.is_empty()), request_query) {
        (Some(left), Some(right)) => {
            url.push('?');
            url.push_str(left);
            url.push('&');
            url.push_str(right);
        }
        (Some(value), None) | (None, Some(value)) => {
            url.push('?');
            url.push_str(value);
        }
        (None, None) => {}
    }
    url
}

pub fn is_self_proxy_target(url: &str, port: u16) -> bool {
    reqwest::Url::parse(url).ok().is_some_and(|parsed| {
        crate::repair::is_localhost(parsed.as_str()) && parsed.port_or_known_default() == Some(port)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Provider;
    use axum::http::HeaderMap;

    #[test]
    fn test_build_target_url_normalization() {
        // Base with /v1, rest with /v1/messages -> should NOT double /v1
        assert_eq!(
            build_target_url("https://api.openai.com/v1", "/v1/chat/completions", None),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            build_target_url("https://api.openai.com/v1/", "/v1/chat/completions", None),
            "https://api.openai.com/v1/chat/completions"
        );
        // Base without /v1, rest with /v1/messages
        assert_eq!(
            build_target_url("https://api.openai.com", "/v1/chat/completions", None),
            "https://api.openai.com/v1/chat/completions"
        );
        // Query param support
        assert_eq!(
            build_target_url(
                "https://api.anthropic.com",
                "/v1/messages",
                Some("beta=true")
            ),
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        assert_eq!(
            build_target_url("  https://api.example/v1  ", "/v1/models", None),
            "https://api.example/v1/models"
        );
    }

    #[test]
    fn self_proxy_target_is_rejected_but_other_local_ports_are_allowed() {
        assert!(is_self_proxy_target(
            "http://127.0.0.1:8999/claude/v1/messages",
            8999
        ));
        assert!(is_self_proxy_target(
            "http://localhost:8999/codex/responses",
            8999
        ));
        assert!(!is_self_proxy_target(
            "http://127.0.0.1:8899/claude/v1/messages",
            8999
        ));
        assert!(!is_self_proxy_target(
            "https://api.example/v1/messages",
            8999
        ));
    }

    #[test]
    fn placeholder_key_check_ignores_surrounding_whitespace() {
        assert!(is_placeholder_key(PLACEHOLDER_KEY));
        assert!(is_placeholder_key("  z-switch-proxy\t"));
        assert!(!is_placeholder_key("z-switch-proxy-other"));
        assert!(!is_placeholder_key(""));
    }

    #[test]
    fn placeholder_auth_check_handles_bearer_spacing_and_case() {
        assert!(is_placeholder_auth_value("  bearer\tz-switch-proxy  "));
        assert!(is_placeholder_auth_value("Bearer  z-switch-proxy"));
        assert!(is_placeholder_auth_value("z-switch-proxy"));
        assert!(!is_placeholder_auth_value("Bearer real-key"));
        assert!(!is_placeholder_auth_value("Basic z-switch-proxy"));
    }

    #[test]
    fn test_target_from_provider_and_proxied_provider() {
        let p = Provider {
            id: "test-claude".into(),
            name: "Test Claude".into(),
            category: Some("custom".into()),
            settings_config: serde_json::json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                    "ANTHROPIC_AUTH_TOKEN": "sk-ant-test"
                }
            }),
            meta: serde_json::json!({ "apiKeyField": "ANTHROPIC_AUTH_TOKEN" }),
            failover: serde_json::json!({ "enabled": false }),
        };

        let target = target_from_provider("claude", &p).unwrap();
        assert_eq!(target.base_url, "https://api.anthropic.com");
        assert_eq!(
            target.headers,
            vec![(
                "authorization".to_string(),
                "Bearer sk-ant-test".to_string()
            )]
        );

        let proxied = proxied_provider("claude", &p, 8999);
        let env = proxied.settings_config.get("env").unwrap();
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").unwrap().as_str().unwrap(),
            "http://127.0.0.1:8999/claude"
        );
        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN").unwrap().as_str().unwrap(),
            PLACEHOLDER_KEY
        );
    }

    #[test]
    fn target_from_provider_rejects_proxy_placeholder_key() {
        let provider = Provider {
            id: "corrupt-claude".into(),
            name: "Corrupt Claude".into(),
            category: Some("custom".into()),
            settings_config: serde_json::json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.example",
                    "ANTHROPIC_AUTH_TOKEN": "  z-switch-proxy  "
                }
            }),
            meta: serde_json::json!({}),
            failover: serde_json::json!({}),
        };

        assert!(target_from_provider("claude", &provider).is_none());
    }

    #[test]
    fn proxied_claude_provider_masks_both_supported_key_fields() {
        let p = Provider {
            id: "test-claude".into(),
            name: "Test Claude".into(),
            category: Some("custom".into()),
            settings_config: serde_json::json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
                    "ANTHROPIC_AUTH_TOKEN": "real-token",
                    "ANTHROPIC_API_KEY": "real-api-key"
                }
            }),
            meta: serde_json::json!({ "apiKeyField": "ANTHROPIC_API_KEY" }),
            failover: serde_json::json!({ "enabled": false }),
        };

        let proxied = proxied_provider("claude", &p, DEFAULT_PORT);
        let env = proxied.settings_config.get("env").unwrap();
        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN")
                .and_then(serde_json::Value::as_str),
            Some(PLACEHOLDER_KEY)
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY")
                .and_then(serde_json::Value::as_str),
            Some(PLACEHOLDER_KEY)
        );
    }

    #[test]
    fn toml_rewrite_matches_exact_keys() {
        let config = "base_url_extra = \"https://keep.example\"\nbase_url = \"https://old.example\" # removed\n";
        let rewritten = rewrite_toml_keys_in_section(
            config,
            "",
            &[("base_url", "http://127.0.0.1:8999/codex")],
        );
        assert!(rewritten.contains("base_url_extra = \"https://keep.example\""));
        assert!(rewritten.contains("base_url = \"http://127.0.0.1:8999/codex\""));
        assert!(!rewritten.contains("old.example"));
    }

    #[test]
    fn codex_proxy_rewrite_does_not_touch_unmanaged_base_urls() {
        let config = "model_provider = \"custom\"\n\n[model_providers.custom] # selected relay\nbase_url = \"https://provider.example\"\n\n[mcp_servers.docs]\nbase_url = \"https://docs.example\"\n";
        let rewritten = rewrite_codex_base_url(config, "http://127.0.0.1:8999/codex");
        assert!(rewritten.contains("base_url = \"http://127.0.0.1:8999/codex\""));
        assert!(rewritten.contains("base_url = \"https://docs.example\""));
    }

    #[test]
    fn codex_proxy_rewrite_ignores_provider_key_in_another_section() {
        let config =
            "[mcp_servers.docs]\nmodel_provider = \"wrong\"\nbase_url = \"https://docs.example\"\n";
        let rewritten = rewrite_codex_base_url(config, "http://127.0.0.1:8999/codex");
        assert!(rewritten.contains("base_url = \"https://docs.example\""));
        assert!(!rewritten.contains("127.0.0.1:8999"));
    }

    #[test]
    fn codex_proxy_rewrite_supports_legacy_root_base_url() {
        let config = r#"model = "gpt-4"
base_url = "https://relay.example/v1"
wire_api = "responses"

[mcp_servers.docs]
base_url = "https://docs.example"
"#;
        let rewritten = rewrite_codex_base_url(config, "http://127.0.0.1:8999/codex");
        assert!(rewritten.contains("base_url = \"http://127.0.0.1:8999/codex\""));
        assert!(rewritten.contains("base_url = \"https://docs.example\""));
    }

    #[test]
    fn codex_proxy_rewrite_does_not_touch_array_table_fields() {
        let config = r#"model_provider = "custom"

[model_providers.custom]
base_url = "https://provider.example"

[[mcp_servers.docs]]
base_url = "https://array.example"
"#;
        let rewritten = rewrite_codex_base_url(config, "http://127.0.0.1:8999/codex");
        assert_eq!(rewritten.matches("http://127.0.0.1:8999/codex").count(), 1);
        assert!(rewritten.contains("base_url = \"https://array.example\""));
    }

    #[test]
    fn grok_proxy_rewrite_does_not_touch_array_table_fields() {
        let provider = Provider {
            id: "grok-array".into(),
            name: "Grok array".into(),
            category: Some("custom".into()),
            settings_config: serde_json::json!({
                "config": "[endpoints]\nmodels_base_url = \"https://provider.example/v1\"\n\n[[mcp_servers.docs]]\nmodels_base_url = \"https://array.example/v1\"\napi_key = \"array-secret\"\n"
            }),
            meta: serde_json::json!({}),
            failover: serde_json::json!({}),
        };

        let proxied = proxied_provider("grok", &provider, DEFAULT_PORT);
        let config = proxied
            .settings_config
            .get("config")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(config.contains("models_base_url = \"http://127.0.0.1:8999/grok\""));
        assert!(config.contains("models_base_url = \"https://array.example/v1\""));
        assert!(config.contains("api_key = \"array-secret\""));
    }

    #[test]
    fn grok_proxy_rewrite_scopes_model_secrets_and_supports_commented_headers() {
        let provider = Provider {
            id: "grok-with-mcp".into(),
            name: "Grok with MCP".into(),
            category: Some("custom".into()),
            settings_config: serde_json::json!({
                "config": "[endpoints] # managed endpoint\nmodels_base_url = \"https://provider.example/v1\"\napi_key = \"endpoint-secret\"\n\n[mcp_servers.docs]\nbase_url = \"https://docs.example\"\napi_key = \"mcp-secret\"\n\n[model.\"grok-4.5\"]\napi_key = \"model-secret\"\n"
            }),
            meta: serde_json::json!({}),
            failover: serde_json::json!({}),
        };

        let proxied = proxied_provider("grok", &provider, DEFAULT_PORT);
        let config = proxied
            .settings_config
            .get("config")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(config.contains("models_base_url = \"http://127.0.0.1:8999/grok\""));
        assert!(config.contains("[endpoints] # managed endpoint"));
        assert!(config.contains("[mcp_servers.docs]"));
        assert!(config.contains("base_url = \"https://docs.example\""));
        assert!(config.contains("api_key = \"mcp-secret\""));
        assert_eq!(config.matches("api_key = \"z-switch-proxy\"").count(), 2);
    }

    #[test]
    fn grok_proxy_rewrite_handles_root_level_legacy_endpoints() {
        let provider = Provider {
            id: "legacy-grok".into(),
            name: "Legacy Grok".into(),
            category: Some("custom".into()),
            settings_config: serde_json::json!({
                "config": "models_base_url = \"https://provider.example/v1\"\napi_key = \"secret-key\"\n"
            }),
            meta: serde_json::json!({}),
            failover: serde_json::json!({}),
        };
        let proxied = proxied_provider("grok", &provider, DEFAULT_PORT);
        let config = proxied
            .settings_config
            .get("config")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(config.contains("models_base_url = \"http://127.0.0.1:8999/grok\""));
        assert!(config.contains("api_key = \"z-switch-proxy\""));
        assert!(!config.contains("provider.example"));
        assert!(!config.contains("secret-key"));
    }

    #[test]
    fn in_flight_guard_does_not_underflow_after_counter_reset() {
        let counter = Arc::new(AtomicU32::new(0));
        let guard = InFlightGuard(Some(counter.clone()));
        counter.store(0, Ordering::Relaxed);
        drop(guard);
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        counter.store(2, Ordering::Relaxed);
        drop(InFlightGuard(Some(counter.clone())));
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn admin_token_is_required_for_control_plane() {
        let mut headers = HeaderMap::new();
        headers.insert("x-z-switch-admin-token", "correct".parse().unwrap());
        assert!(authorized(&headers, &runtime_for_test("correct")));

        headers.insert("x-z-switch-admin-token", "wrong".parse().unwrap());
        assert!(!authorized(&headers, &runtime_for_test("correct")));
        assert!(!authorized(&HeaderMap::new(), &runtime_for_test("correct")));
    }

    #[test]
    fn connection_header_declared_names_are_treated_as_hop_by_hop() {
        let names = hop_by_hop_headers([
            "X-Request-Only, keep-alive".to_string(),
            "x-response-only".to_string(),
        ]);
        assert!(names.contains("connection"));
        assert!(names.contains("x-request-only"));
        assert!(names.contains("x-response-only"));
        assert!(names.contains("keep-alive"));
    }

    #[test]
    fn forward_headers_preserves_repeated_client_values_and_target_override() {
        let mut headers = HeaderMap::new();
        headers.append("cookie", "session=one".parse().unwrap());
        headers.append("cookie", "theme=dark".parse().unwrap());
        headers.append("accept", "text/event-stream".parse().unwrap());
        headers.insert("authorization", "Bearer client".parse().unwrap());
        headers.insert("connection", "x-request-only".parse().unwrap());
        headers.insert("x-request-only", "must-not-forward".parse().unwrap());

        let forwarded = forward_headers(
            &headers,
            &[
                ("cookie".into(), "session=target".into()),
                ("x-target".into(), "one".into()),
                ("x-target".into(), "two".into()),
            ],
        );

        let cookies: Vec<_> = forwarded
            .get_all("cookie")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(cookies, vec!["session=target"]);
        let target_values: Vec<_> = forwarded
            .get_all("x-target")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(target_values, vec!["one", "two"]);
        assert_eq!(forwarded.get_all("accept").iter().count(), 1);
        assert!(forwarded.get("authorization").is_none());
        assert!(forwarded.get("connection").is_none());
        assert!(forwarded.get("x-request-only").is_none());
    }

    fn runtime_for_test(token: &str) -> Runtime {
        Runtime {
            client: reqwest::Client::new(),
            targets: Arc::new(RwLock::new(ProxyTargets::default())),
            config: ProxyRuntimeConfig::default(),
            error_log_lock: tokio::sync::Mutex::new(()),
            counters: HashMap::new(),
            port: DEFAULT_PORT,
            shutdown_sender: mpsc::channel(1).0,
            admin_token: token.to_string(),
        }
    }
}
