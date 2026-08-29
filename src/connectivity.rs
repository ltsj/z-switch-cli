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
    let mut out = vec![format!("{b}/v1/models"), format!("{b}/models")];
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
                out.push(format!("{root}/v1/models"));
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
        let mut req = client.get(&url);
        if !api_key.trim().is_empty() {
            req = req.bearer_auth(api_key.trim());
        }
        match req.send().await {
            Ok(resp) => {
                let ms = start.elapsed().as_millis() as u64;
                let code = resp.status();
                if code == reqwest::StatusCode::UNAUTHORIZED
                    || code == reqwest::StatusCode::FORBIDDEN
                {
                    return Ok(ConnResult {
                        status: "unauthorized".into(),
                        detail: format!("地址可达，但 API Key 被拒 (HTTP {})", code.as_u16()),
                        ms: Some(ms),
                    });
                }
                return Ok(ConnResult {
                    status: "ok".into(),
                    detail: format!("已连通 (HTTP {})", code.as_u16()),
                    ms: Some(ms),
                });
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
    Ok(ConnResult {
        status: "unreachable".into(),
        detail: last_err,
        ms: None,
    })
}
