#![allow(dead_code)]
//! 拉取供应商可用模型列表（OpenAI 兼容 GET /v1/models）。
use serde::Deserialize;

#[derive(Deserialize)]
struct ModelsResp {
    data: Vec<ModelEntry>,
}
#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

fn candidates(base_url: &str, models_url: Option<&str>) -> Vec<String> {
    if let Some(u) = models_url {
        let u = u.trim();
        if !u.is_empty() {
            return vec![u.to_string()];
        }
    }
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
                out.push(crate::proxy::build_target_url(root, "/models", None));
            }
        }
    }
    out.dedup();
    out
}

pub async fn fetch_models(
    base_url: &str,
    api_key: &str,
    models_url: Option<&str>,
) -> Result<Vec<String>, String> {
    fetch_models_with_auth(base_url, api_key, models_url, None).await
}

/// 拉取模型列表，并按客户端实际使用的方式发送鉴权信息。
///
/// Claude 的 `ANTHROPIC_API_KEY` 使用 `x-api-key`，而
/// `ANTHROPIC_AUTH_TOKEN`、Codex 与 Grok 使用 Bearer。保留上面的
/// `fetch_models` 作为默认 Bearer 入口，兼容原项目已有调用方。
pub async fn fetch_models_with_auth(
    base_url: &str,
    api_key: &str,
    models_url: Option<&str>,
    api_key_field: Option<&str>,
) -> Result<Vec<String>, String> {
    let urls = candidates(base_url, models_url);
    if urls.is_empty() {
        return Err("请先填写 Base URL".into());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(12))
        .build()
        .map_err(|e| e.to_string())?;

    let mut last_err = String::from("无可用端点");
    for url in urls {
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
                let status = resp.status();
                if !status.is_success() {
                    last_err = format!("{url} 返回 {status}");
                    continue;
                }
                match resp.json::<ModelsResp>().await {
                    Ok(m) if !m.data.is_empty() => {
                        let mut ids: Vec<String> = m.data.into_iter().map(|e| e.id).collect();
                        ids.sort();
                        ids.dedup();
                        return Ok(ids);
                    }
                    Ok(_) => {
                        last_err = format!("{url} 返回空列表");
                    }
                    Err(e) => {
                        last_err = format!("{url} 解析失败: {e}");
                    }
                }
            }
            Err(e) => {
                last_err = format!("{url} 请求失败: {e}");
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::{candidates, fetch_models_with_auth};
    use axum::{extract::State, http::HeaderMap, routing::get, Router};
    use std::sync::{Arc, Mutex};

    type CapturedHeaders = Arc<Mutex<Option<(Option<String>, Option<String>)>>>;

    #[test]
    fn candidates_preserve_base_query_and_deduplicate_v1() {
        let urls = candidates("https://api.example/v1?api-version=2026", None);
        assert!(urls.contains(&"https://api.example/v1/models?api-version=2026".to_string()));
        assert!(!urls.iter().any(|url| url.contains("/v1/v1/")));
    }

    #[tokio::test]
    async fn claude_api_key_uses_x_api_key_header() {
        let observed: CapturedHeaders = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route(
                "/v1/models",
                get(
                    |State(observed): State<CapturedHeaders>, headers: HeaderMap| async move {
                        *observed.lock().expect("header capture lock") = Some((
                            headers
                                .get("x-api-key")
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_string),
                            headers
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_string),
                        ));
                        axum::Json(serde_json::json!({
                            "data": [{ "id": "claude-sonnet" }]
                        }))
                    },
                ),
            )
            .with_state(observed.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind model test server");
        let address = listener.local_addr().expect("model test server address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let models = fetch_models_with_auth(
            &format!("http://{address}"),
            "claude-key",
            None,
            Some("ANTHROPIC_API_KEY"),
        )
        .await
        .expect("model list response");

        assert_eq!(models, vec!["claude-sonnet"]);
        assert_eq!(
            *observed.lock().expect("header capture lock"),
            Some((Some("claude-key".into()), None))
        );
        server.abort();
    }
}
