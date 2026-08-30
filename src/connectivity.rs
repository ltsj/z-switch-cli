#![allow(dead_code)]
//! 连通性与延迟测试。
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ConnResult {
    /// ok = 地址可达且 key 被接受；unauthorized = 地址通但 key 无效；unreachable = 连不上/超时
    pub status: String,
    /// 说明（含 HTTP 码或错误原因）
    pub detail: String,
    /// 往返毫秒
    pub ms: Option<u64>,
}

fn probe_urls(base_url: &str) -> Vec<String> {
    let b = base_url.trim().trim_end_matches('/');
    if b.is_empty() {
        return vec![];
    }
    let mut out = vec![
        crate::proxy::build_target_url(b, "/v1/models", None),
        crate::proxy::build_target_url(b, "/models", None),
    ];
    for suffix in [
        "/anthropic",
        "/v1",
        "/compatible",
        "/api/anthropic",
        "/openai",
    ] {
        if let Some(root) = b.strip_suffix(suffix) {
            let root = root.trim_end_matches('/');
            if !root.is_empty() {
                out.push(crate::proxy::build_target_url(root, "/v1/models", None));
            }
        }
    }
    out.dedup();
    out
}

/// 端点测速：HTTP 层探测，测真实往返耗时（毫秒）。
pub async fn latency(base_url: &str) -> Result<f64, String> {
    let urls = probe_urls(base_url);
    if urls.is_empty() {
        return Err("请先填写 Base URL".into());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut last_err = String::from("无法连接");
    for url in urls {
        let start = std::time::Instant::now();
        match client.get(&url).send().await {
            Ok(_resp) => return Ok(start.elapsed().as_secs_f64() * 1000.0),
            Err(e) => {
                last_err = if e.is_timeout() {
                    format!("{url} 超时")
                } else {
                    format!("{url} 连接失败")
                };
            }
        }
    }
    Err(last_err)
}

/// 测试连通性
pub async fn test(base_url: &str, api_key: &str) -> Result<ConnResult, String> {
    test_with_auth(base_url, api_key, None).await
}

/// 测试连通性，并按客户端实际使用的鉴权方式发送探测请求。
///
/// Claude 的 ANTHROPIC_API_KEY 必须通过 x-api-key 发送；其它应用以及
/// Claude 的 ANTHROPIC_AUTH_TOKEN 使用 Bearer。保留 test() 作为默认
/// Bearer 入口，兼容已有调用方。
pub async fn test_with_auth(
    base_url: &str,
    api_key: &str,
    api_key_field: Option<&str>,
) -> Result<ConnResult, String> {
    let urls = probe_urls(base_url);
    if urls.is_empty() {
        return Err("请先填写 Base URL".into());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut last_err = String::from("无法连接");
    let mut last_http_error = None;
    let mut last_unauthorized = None;
    for url in urls {
        let start = std::time::Instant::now();
        let mut req = client.get(&url);
        if !api_key.trim().is_empty() {
            if api_key_field == Some("ANTHROPIC_API_KEY") {
                req = req.header("x-api-key", api_key.trim());
            } else {
                req = req.bearer_auth(api_key.trim());
            }
        }
        match req.send().await {
            Ok(resp) => {
                let ms = start.elapsed().as_millis() as u64;
                let code = resp.status();
                if code.is_success() {
                    return Ok(ConnResult {
                        status: "ok".into(),
                        detail: format!("已连通 (HTTP {})", code.as_u16()),
                        ms: Some(ms),
                    });
                }
                if code == reqwest::StatusCode::UNAUTHORIZED
                    || code == reqwest::StatusCode::FORBIDDEN
                {
                    // A gateway may return 401/403 for an unsupported probe
                    // path instead of 404. Continue through the compatible
                    // candidates before deciding that the key is invalid.
                    last_unauthorized = Some((url, code.as_u16(), ms));
                    continue;
                }
                // 中转服务对 /v1/models 与 /models 的支持并不一致。404、
                // 5xx 等状态只能说明当前候选路径不可用，继续尝试其它路径。
                last_http_error = Some((url, code.as_u16(), ms));
            }
            Err(e) => {
                last_err = if e.is_timeout() {
                    format!("{url} 超时")
                } else {
                    format!("{url} 连接失败")
                };
            }
        }
    }
    if let Some((url, code, ms)) = last_http_error {
        return Ok(ConnResult {
            status: "http_error".into(),
            detail: format!("地址可达，但兼容探测路径均不可用（最后响应 {url}: HTTP {code}）"),
            ms: Some(ms),
        });
    }
    if let Some((url, code, ms)) = last_unauthorized {
        return Ok(ConnResult {
            status: "unauthorized".into(),
            detail: format!("地址可达，但 API Key 被拒 (最后响应 {url}: HTTP {code})"),
            ms: Some(ms),
        });
    }
    Ok(ConnResult {
        status: "unreachable".into(),
        detail: last_err,
        ms: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{probe_urls, test_with_auth};
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::get,
        Router,
    };

    async fn spawn_probe_server(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), task)
    }

    #[test]
    fn probe_urls_preserves_base_query_and_deduplicates_v1() {
        let urls = probe_urls("https://api.example/v1?api-version=2026");
        assert!(urls.contains(&"https://api.example/v1/models?api-version=2026".to_string()));
        assert!(!urls.iter().any(|url| url.contains("/v1/v1/")));
    }

    #[tokio::test]
    async fn connectivity_probe_tries_the_next_compatible_path() {
        let app = Router::new()
            .route("/v1/models", get(|| async { StatusCode::NOT_FOUND }))
            .route("/models", get(|| async { StatusCode::OK }));
        let (base_url, server) = spawn_probe_server(app).await;

        let result = test_with_auth(&base_url, "test-key", None)
            .await
            .expect("probe result");
        assert_eq!(result.status, "ok");
        assert!(result.detail.contains("HTTP 200"));

        server.abort();
    }

    #[tokio::test]
    async fn connectivity_probe_reports_non_success_http_responses() {
        let app = Router::new()
            .route("/v1/models", get(|| async { StatusCode::NOT_FOUND }))
            .route(
                "/models",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            );
        let (base_url, server) = spawn_probe_server(app).await;

        let result = test_with_auth(&base_url, "test-key", None)
            .await
            .expect("probe result");
        assert_eq!(result.status, "http_error");
        assert!(result.detail.contains("HTTP 500"));

        server.abort();
    }

    #[tokio::test]
    async fn connectivity_probe_does_not_stop_on_unauthorized_probe_path() {
        let app = Router::new()
            .route("/v1/models", get(|| async { StatusCode::UNAUTHORIZED }))
            .route("/models", get(|| async { StatusCode::OK }));
        let (base_url, server) = spawn_probe_server(app).await;

        let result = test_with_auth(&base_url, "test-key", None)
            .await
            .expect("probe result");
        assert_eq!(result.status, "ok");
        assert!(result.detail.contains("HTTP 200"));

        server.abort();
    }

    #[tokio::test]
    async fn connectivity_probe_uses_the_requested_auth_header() {
        let app = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                if headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok())
                    == Some("claude-key")
                    && !headers.contains_key("authorization")
                {
                    StatusCode::OK
                } else {
                    StatusCode::UNAUTHORIZED
                }
            }),
        );
        let (base_url, server) = spawn_probe_server(app).await;

        let result = test_with_auth(&base_url, "claude-key", Some("ANTHROPIC_API_KEY"))
            .await
            .expect("probe result");
        assert_eq!(result.status, "ok");

        server.abort();
    }
}
