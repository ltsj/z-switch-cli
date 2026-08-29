//! 可选热切换代理。
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::Response,
    routing::any,
    Router,
};
use futures_util::StreamExt;
use tokio::sync::oneshot;

use crate::proxy_log;

pub const DEFAULT_PORT: u16 = 8899;

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

#[derive(Clone, Default, Debug)]
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

pub const PROXY_APPS: &[&str] = &["claude", "codex"];

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
}

struct InFlightGuard(Option<Arc<AtomicU32>>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Some(counter) = &self.0 {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

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

    pub async fn start(&mut self, port: u16, config: ProxyRuntimeConfig) -> Result<(), String> {
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
        let runtime = Arc::new(Runtime {
            client,
            targets: self.handle.targets.clone(),
            config,
            error_log_lock: tokio::sync::Mutex::new(()),
            counters: self.handle.counters.clone(),
        });

        let app = Router::new()
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
                let _ = rx.await;
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

    let base = target.base_url.trim_end_matches('/');
    let mut url = format!("{base}{rest}");
    if let Some(q) = uri.query() {
        url.push('?');
        url.push_str(q);
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

    let mut fwd = reqwest::header::HeaderMap::new();
    for (name, value) in headers.iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str()) {
            continue;
        }
        if AUTH_HEADERS.contains(&lname.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            fwd.insert(n, v);
        }
    }
    for (k, v) in &target.headers {
        if let (Ok(n), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            fwd.insert(n, val);
        }
    }

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
    for (name, value) in upstream.headers().iter() {
        let lname = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str()) {
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
        (upstream_stream, true, false, Vec::<u8>::new(), in_flight_guard),
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
    match app {
        "claude" => {
            let env = provider.settings_config.get("env")?.as_object()?;
            let base = env.get("ANTHROPIC_BASE_URL")?.as_str()?.trim().to_string();
            if base.is_empty() {
                return None;
            }
            let key_field = provider
                .meta
                .get("apiKeyField")
                .and_then(|v| v.as_str())
                .unwrap_or("ANTHROPIC_AUTH_TOKEN");
            let key = env.get(key_field).and_then(|v| v.as_str()).unwrap_or("");
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
            let base = cfg
                .lines()
                .find_map(|l| l.trim().strip_prefix("base_url"))
                .and_then(|r| r.split('"').nth(1))
                .map(|s| s.trim().to_string())?;
            if base.is_empty() {
                return None;
            }
            let key = provider
                .settings_config
                .get("auth")
                .and_then(|a| a.get("OPENAI_API_KEY"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
    }
}

pub const PLACEHOLDER_KEY: &str = "z-switch-proxy";

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

pub fn proxied_provider(
    app: &str,
    provider: &crate::store::Provider,
    port: u16,
) -> crate::store::Provider {
    if crate::store::is_official_provider(provider) {
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
                let key_field = provider
                    .meta
                    .get("apiKeyField")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ANTHROPIC_AUTH_TOKEN")
                    .to_string();
                env.insert(
                    key_field,
                    serde_json::Value::String(PLACEHOLDER_KEY.to_string()),
                );
            }
        }
        "codex" => {
            if let Some(cfg) = p.settings_config.get("config").and_then(|v| v.as_str()) {
                let rewritten: String = cfg
                    .lines()
                    .map(|line| {
                        if line.trim().starts_with("base_url") {
                            format!("base_url = \"{local}\"")
                        } else {
                            line.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
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
        _ => {}
    }
    p
}
