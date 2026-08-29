//! 终端 UI 渲染、Inquire 交互式菜单与表格格式化输出。
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use inquire::{Confirm, Select, Text};
use std::time::Duration;

use crate::connectivity;
use crate::daemon;
use crate::proxy::DEFAULT_PORT;
use crate::service::SwitchService;
use crate::store::{self, AppData, Provider};
use crate::stream_test;

pub fn print_banner() {
    println!(
        "{}",
        "  ███████╗      ███████╗██╗    ██╗██╗████████╗ ██████╗██╗  ██╗"
            .bright_cyan()
            .bold()
    );
    println!(
        "{}",
        "  ╚══███╔╝      ██╔════╝██║    ██║██║╚══██╔══╝██╔════╝██║  ██║"
            .bright_cyan()
            .bold()
    );
    println!(
        "{}",
        "    ███╔╝ █████╗███████╗██║ █╗ ██║██║   ██║   ██║     ███████║"
            .bright_cyan()
            .bold()
    );
    println!(
        "{}",
        "   ███╔╝  ╚════╝╚════██║██║███╗██║██║   ██║   ██║     ██╔══██║"
            .bright_cyan()
            .bold()
    );
    println!(
        "{}",
        "  ███████╗      ███████║╚███╔███╔╝██║   ██║   ╚██████╗██║  ██║"
            .bright_cyan()
            .bold()
    );
    println!(
        "{}",
        "  ╚══════╝      ╚══════╝ ╚══╝╚══╝ ╚═╝   ╚═╝    ╚═════╝╚═╝  ╚═╝"
            .bright_cyan()
            .bold()
    );
    println!(
        "  {} {}",
        "z-switch-cli · Claude Code / Codex / Grok 供应商管理工具"
            .bright_white()
            .bold(),
        format!("v{}", env!("CARGO_PKG_VERSION")).green()
    );
    println!();
}

pub fn format_app_title(app: &str) -> String {
    match app {
        "claude" => format!("🤖 Claude Code ({})", "~/.claude/settings.json".dimmed()),
        "codex" => format!("⚡ Codex CLI ({})", "~/.codex/config.toml".dimmed()),
        "grok" => format!("🌌 Grok CLI ({})", "~/.grok/config.toml".dimmed()),
        other => other.to_string(),
    }
}

pub fn print_providers_table(app: &str, data: &AppData, proxy_alive: bool) {
    println!("{}", format_app_title(app).bold());
    println!("{}", "─".repeat(80).dimmed());

    println!(
        " {:<3} {:<24} {:<18} {:<32}",
        "状态".bright_yellow(),
        "供应商名称 (ID)".bright_yellow(),
        "模型 (Model)".bright_yellow(),
        "Base URL".bright_yellow(),
    );
    println!("{}", "─".repeat(80).dimmed());

    if data.order.is_empty() {
        println!("  {}", "暂无供应商配置，可通过 add 或 import 导入".dimmed());
        println!();
        return;
    }

    for id in &data.order {
        if let Some(p) = data.providers.get(id) {
            let is_current = data.current.as_deref() == Some(id.as_str());
            let status_mark = if is_current {
                if proxy_alive && !store::is_official_provider(p) {
                    "● 代理".bright_green().bold()
                } else {
                    "● 直连".bright_cyan().bold()
                }
            } else {
                "○     ".dimmed()
            };

            let name_display = if p.name.len() > 22 {
                format!("{}...", &p.name[..19])
            } else {
                p.name.clone()
            };

            let name_and_id = if store::is_official_provider(p) {
                format!("{} {}", name_display.bright_yellow(), "[官方]".dimmed())
            } else if p.id == name_display {
                name_display
            } else {
                format!("{name_display} ({})", p.id.dimmed())
            };

            let model_display = p
                .extract_model(app)
                .unwrap_or_else(|| "-".to_string())
                .chars()
                .take(16)
                .collect::<String>();

            let base_url_display = p
                .extract_base_url(app)
                .unwrap_or_else(|| {
                    if store::is_official_provider(p) {
                        "https://api.anthropic.com (默认官方)".into()
                    } else {
                        "-".into()
                    }
                })
                .chars()
                .take(34)
                .collect::<String>();

            println!(
                " {:<6} {:<24} {:<18} {:<32}",
                status_mark,
                if is_current {
                    name_and_id.bold()
                } else {
                    name_and_id.normal()
                },
                model_display.dimmed(),
                base_url_display.dimmed(),
            );
        }
    }
    println!("{}", "─".repeat(80).dimmed());
    println!();
}

pub async fn run_interactive_menu(service: &SwitchService) {
    print_banner();

    loop {
        let options = vec![
            "🚀 一键切换供应商 (Switch Provider)",
            "📋 查看所有供应商列表 (List Providers)",
            "⚡ 真实流式测速与验真 (Speed Test & TTFT)",
            "➕ 新增供应商 (Add Provider)",
            "✏️ 编辑供应商配置 (Edit Provider)",
            "🗑️ 删除供应商 (Delete Provider)",
            "🌐 本地代理守护进程管理 (Proxy Daemon)",
            "🔄 导入供应商 (Import cc-switch / live)",
            "🩺 环境诊断与自愈 (Doctor & Repair)",
            "⚙️ 系统偏好设置 (Settings)",
            "🚪 退出 (Exit)",
        ];

        let ans = match Select::new("请选择操作:", options).prompt() {
            Ok(choice) => choice,
            Err(_) => break,
        };

        match ans {
            opt if opt.starts_with("🚀") => {
                interactive_switch(service).await;
            }
            opt if opt.starts_with("📋") => {
                interactive_list(service).await;
            }
            opt if opt.starts_with("⚡") => {
                interactive_test(service).await;
            }
            opt if opt.starts_with("➕") => {
                interactive_add(service).await;
            }
            opt if opt.starts_with("✏️") => {
                interactive_edit(service).await;
            }
            opt if opt.starts_with("🗑️") => {
                interactive_remove(service).await;
            }
            opt if opt.starts_with("🌐") => {
                interactive_proxy_menu().await;
            }
            opt if opt.starts_with("🔄") => {
                interactive_import(service).await;
            }
            opt if opt.starts_with("🩺") => {
                interactive_doctor(service).await;
            }
            opt if opt.starts_with("⚙️") => {
                interactive_settings(service).await;
            }
            _ => {
                println!("{}", "感谢使用 z-switch-cli，再见！".bright_green());
                break;
            }
        }
    }
}

pub async fn interactive_list(service: &SwitchService) {
    let root = service.get_root();
    let proxy_alive = daemon::is_running(DEFAULT_PORT).await;
    println!();
    for &app in &["claude", "codex", "grok"] {
        if let Some(data) = root.apps.get(app) {
            print_providers_table(app, data, proxy_alive);
        }
    }
}

pub async fn interactive_switch(service: &SwitchService) {
    let app_choice = match Select::new(
        "选择要切换的目标应用:",
        vec![
            "claude (Claude Code)",
            "codex (Codex CLI)",
            "grok (Grok CLI)",
        ],
    )
    .prompt()
    {
        Ok(s) => s.split_whitespace().next().unwrap_or("claude").to_string(),
        Err(_) => return,
    };

    let root = service.get_root();
    let data = match root.apps.get(&app_choice) {
        Some(d) if !d.order.is_empty() => d,
        _ => {
            println!("{}", "该应用暂无可用供应商，请先添加".bright_red());
            return;
        }
    };

    let mut provider_items = Vec::new();
    for id in &data.order {
        if let Some(p) = data.providers.get(id) {
            let is_current = data.current.as_deref() == Some(id.as_str());
            let mark = if is_current { " [当前生效]" } else { "" };
            let model = p
                .extract_model(&app_choice)
                .map(|m| format!(" ({m})"))
                .unwrap_or_default();
            provider_items.push(format!("{}{} - {}{}", p.name, model, p.id, mark));
        }
    }

    let selected_str = match Select::new("选择目标供应商:", provider_items).prompt() {
        Ok(s) => s,
        Err(_) => return,
    };

    let id_part = selected_str
        .split(" - ")
        .nth(1)
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("");

    let mode_choice = match Select::new(
        "选择运行模式:",
        vec![
            "⚡ 智能推荐 / 本地代理热切 (随时命令行热更，无感知)",
            "🔗 原生直连模式 (直接将 Base URL 和 Key 写入客户端配置)",
        ],
    )
    .prompt()
    {
        Ok(s) => s.starts_with("⚡"),
        Err(_) => return,
    };

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.green} {msg}")
            .unwrap(),
    );
    spinner.set_message("正在应用切换配置...");
    spinner.enable_steady_tick(Duration::from_millis(80));

    match service
        .switch_async(&app_choice, id_part, Some(mode_choice), Some(DEFAULT_PORT))
        .await
    {
        Ok((p, proxied)) => {
            spinner.finish_and_clear();
            let mode_str = if proxied {
                "【本地代理热切模式】".bright_green().bold()
            } else {
                "【直连模式】".bright_cyan().bold()
            };
            println!(
                "{} 成功将 {} 切换至供应商：{} {}",
                "✔".bright_green().bold(),
                app_choice.bright_yellow(),
                p.name.bright_white().bold(),
                mode_str
            );
            if proxied {
                println!(
                    "  {} 代理正在 127.0.0.1:{} 转发请求，无需重启终端即可生效！",
                    "ℹ".bright_blue(),
                    DEFAULT_PORT
                );
            }
        }
        Err(e) => {
            spinner.finish_and_clear();
            println!("{} 切换失败: {e}", "✖".bright_red().bold());
        }
    }
    println!();
}

pub async fn interactive_test(service: &SwitchService) {
    let app_choice = match Select::new(
        "选择要测速的目标应用:",
        vec!["claude (Claude Code)", "codex (Codex)", "grok (Grok)"],
    )
    .prompt()
    {
        Ok(s) => s.split_whitespace().next().unwrap_or("claude").to_string(),
        Err(_) => return,
    };

    let root = service.get_root();
    let data = match root.apps.get(&app_choice) {
        Some(d) if !d.order.is_empty() => d,
        _ => {
            println!("{}", "该应用暂无可用供应商".bright_red());
            return;
        }
    };

    let mut provider_items = vec!["🔥 测试全部供应商 (Batch Test All)".to_string()];
    for id in &data.order {
        if let Some(p) = data.providers.get(id) {
            provider_items.push(format!("{} ({})", p.name, p.id));
        }
    }

    let choice = match Select::new("选择测速对象:", provider_items).prompt() {
        Ok(s) => s,
        Err(_) => return,
    };

    if choice.starts_with("🔥") {
        for id in &data.order {
            if let Some(p) = data.providers.get(id) {
                test_single_provider(&app_choice, p).await;
            }
        }
    } else {
        let id_part = choice.split('(').nth(1).unwrap_or("").trim_end_matches(')');
        if let Some(p) = data.providers.get(id_part) {
            test_single_provider(&app_choice, p).await;
        }
    }
}

pub async fn test_single_provider(app: &str, provider: &Provider) {
    if store::is_official_provider(provider) {
        println!(
            "{} 官方账号使用的是本机登录态，跳过中转 API 测速",
            "ℹ".bright_blue()
        );
        return;
    }

    let base_url = provider.extract_base_url(app).unwrap_or_default();
    let api_key = provider.extract_api_key(app).unwrap_or_default();
    let model = provider.extract_model(app).unwrap_or_default();
    let wire_api = provider.extract_wire_api(app);
    let key_field = provider.extract_api_key_field(app);

    if base_url.is_empty() {
        println!("{} 供应商 {} 缺少 Base URL", "✖".bright_red(), provider.name);
        return;
    }

    println!();
    println!(
        "{} 正在测试供应商：{} (模型: {})",
        "⚡".bright_yellow(),
        provider.name.bright_white().bold(),
        if model.is_empty() {
            "未指定"
        } else {
            &model
        }
        .bright_cyan()
    );

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
            .template("{spinner:.yellow} {msg}")
            .unwrap(),
    );
    spinner.set_message("探测 HTTP 连通性与网络延迟...");
    spinner.enable_steady_tick(Duration::from_millis(80));

    // HTTP 探测
    let conn_res = connectivity::test(&base_url, &api_key).await;
    spinner.finish_and_clear();

    match conn_res {
        Ok(res) => {
            if res.status == "ok" {
                println!(
                    "  {} HTTP 连通状态: {} ({} ms)",
                    "✔".bright_green(),
                    res.detail.bright_green(),
                    res.ms.unwrap_or(0).to_string().bright_yellow()
                );
            } else if res.status == "unauthorized" {
                println!(
                    "  {} 鉴权受拒: {} ({} ms)",
                    "⚠".bright_yellow(),
                    res.detail.bright_yellow(),
                    res.ms.unwrap_or(0)
                );
            } else {
                println!("  {} 连接失败: {}", "✖".bright_red(), res.detail);
                return;
            }
        }
        Err(e) => {
            println!("  {} 探测异常: {e}", "✖".bright_red());
            return;
        }
    }

    // 真实流式 TTFT 测速
    if !model.is_empty() && !api_key.is_empty() {
        let stream_spinner = ProgressBar::new_spinner();
        stream_spinner.set_style(
            ProgressStyle::default_spinner()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
                .template("{spinner:.green} {msg}")
                .unwrap(),
        );
        stream_spinner.set_message("发起真实流式请求并测量首字延迟 (TTFT)...");
        stream_spinner.enable_steady_tick(Duration::from_millis(80));

        let start = std::time::Instant::now();
        let stream_res = stream_test::run(
            app,
            &base_url,
            &api_key,
            &model,
            &wire_api,
            key_field.as_deref(),
            Some(|_evt: stream_test::StreamTestEvent| {}),
        )
        .await;

        stream_spinner.finish_and_clear();

        match stream_res {
            Ok(result) => {
                println!(
                    "  {} 流式首字延迟 (TTFT): {} ms | 总往返: {} ms",
                    "✔".bright_green().bold(),
                    result.first_token_ms.to_string().bright_green().bold(),
                    result.total_ms.to_string().bright_yellow()
                );
                let preview = result
                    .text
                    .replace('\n', " ")
                    .chars()
                    .take(60)
                    .collect::<String>();
                println!("  {} 模型流式返回片段: \"{}\"", "💬".dimmed(), preview.dimmed());
            }
            Err(e) => {
                println!(
                    "  {} 流式调用失败 (耗时 {} ms): {e}",
                    "✖".bright_red(),
                    start.elapsed().as_millis()
                );
            }
        }
    }
}

pub async fn interactive_add(service: &SwitchService) {
    let app_choice = match Select::new(
        "选择要添加的目标应用:",
        vec!["claude (Claude Code)", "codex (Codex)", "grok (Grok)"],
    )
    .prompt()
    {
        Ok(s) => s.split_whitespace().next().unwrap_or("claude").to_string(),
        Err(_) => return,
    };

    let name = match Text::new("供应商名称 (例如: DeepSeek / GLM-4):").prompt() {
        Ok(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => return,
    };

    let id = name
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric(), "-");

    let url = match Text::new("接口 Base URL (例如: https://api.deepseek.com):").prompt() {
        Ok(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => return,
    };

    let key = match Text::new("API Key / Auth Token:").prompt() {
        Ok(k) => k.trim().to_string(),
        _ => return,
    };

    let model = match Text::new("默认模型 (可选，例如 claude-3-7-sonnet-20250219 / deepseek-chat):").prompt() {
        Ok(m) => m.trim().to_string(),
        _ => String::new(),
    };

    let provider = match app_choice.as_str() {
        "claude" => {
            let key_field = match Select::new(
                "API Key Header 字段类型:",
                vec![
                    "ANTHROPIC_AUTH_TOKEN (Authorization: Bearer <key>)",
                    "ANTHROPIC_API_KEY (x-api-key: <key>)",
                ],
            )
            .prompt()
            {
                Ok(s) if s.starts_with("ANTHROPIC_API_KEY") => "ANTHROPIC_API_KEY",
                _ => "ANTHROPIC_AUTH_TOKEN",
            };

            let mut env = serde_json::json!({
                "ANTHROPIC_BASE_URL": url,
                key_field: key,
            });
            if !model.is_empty() {
                if let Some(obj) = env.as_object_mut() {
                    obj.insert("ANTHROPIC_MODEL".into(), serde_json::Value::String(model.clone()));
                }
            }
            Provider {
                id,
                name,
                category: Some("custom".into()),
                settings_config: serde_json::json!({ "env": env }),
                meta: serde_json::json!({ "apiKeyField": key_field }),
                failover: serde_json::json!({ "enabled": false }),
            }
        }
        "codex" => {
            let toml_config = format!(
                "model_provider = \"custom\"\nmodel = \"{model}\"\n\n[model_providers.custom]\nbase_url = \"{url}\"\nwire_api = \"responses\"\n"
            );
            Provider {
                id,
                name,
                category: Some("custom".into()),
                settings_config: serde_json::json!({
                    "auth": { "OPENAI_API_KEY": key },
                    "config": toml_config
                }),
                meta: serde_json::json!({ "wireApi": "responses" }),
                failover: serde_json::json!({ "enabled": false }),
            }
        }
        "grok" => {
            let toml_config = format!("models_base_url = \"{url}\"\nmodel = \"{model}\"\n");
            Provider {
                id,
                name,
                category: Some("custom".into()),
                settings_config: serde_json::json!({
                    "auth": { "GROK_API_KEY": key },
                    "config": toml_config
                }),
                meta: serde_json::json!({}),
                failover: serde_json::json!({ "enabled": false }),
            }
        }
        _ => return,
    };

    match service.save_provider(&app_choice, provider) {
        Ok(p) => {
            println!(
                "{} 成功添加供应商：{}",
                "✔".bright_green().bold(),
                p.name.bright_white().bold()
            );
        }
        Err(e) => {
            println!("{} 添加失败: {e}", "✖".bright_red().bold());
        }
    }
    println!();
}

pub async fn interactive_edit(service: &SwitchService) {
    let app_choice = match Select::new(
        "选择目标应用:",
        vec![
            "claude (Claude Code)",
            "codex (Codex CLI)",
            "grok (Grok CLI)",
        ],
    )
    .prompt()
    {
        Ok(s) => s.split_whitespace().next().unwrap_or("claude").to_string(),
        Err(_) => return,
    };

    let root = service.get_root();
    let data = match root.apps.get(&app_choice) {
        Some(d) if !d.order.is_empty() => d,
        _ => {
            println!("{}", "该应用暂无可编辑的供应商".bright_red());
            return;
        }
    };

    let mut provider_items = Vec::new();
    for id in &data.order {
        if let Some(p) = data.providers.get(id) {
            if store::is_official_provider(p) {
                continue;
            }
            provider_items.push(format!("{} ({})", p.name, p.id));
        }
    }

    if provider_items.is_empty() {
        println!("{}", "没有可编辑的第三方供应商".bright_yellow());
        return;
    }

    let choice = match Select::new("选择要修改的供应商:", provider_items).prompt() {
        Ok(s) => s,
        Err(_) => return,
    };

    let id_part = choice.split('(').nth(1).unwrap_or("").trim_end_matches(')');
    let old_p = match data.providers.get(id_part) {
        Some(p) => p.clone(),
        None => return,
    };

    let current_name = old_p.name.clone();
    let current_url = old_p.extract_base_url(&app_choice).unwrap_or_default();
    let current_key = old_p.extract_api_key(&app_choice).unwrap_or_default();
    let current_model = old_p.extract_model(&app_choice).unwrap_or_default();

    let new_name = match Text::new("修改名称 (回车保持原样):")
        .with_default(&current_name)
        .prompt()
    {
        Ok(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => return,
    };

    let new_url = match Text::new("修改 Base URL (回车保持原样):")
        .with_default(&current_url)
        .prompt()
    {
        Ok(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => return,
    };

    let new_key = match Text::new("修改 API Key / Auth Token (回车保持原样):")
        .with_default(&current_key)
        .prompt()
    {
        Ok(k) => k.trim().to_string(),
        _ => return,
    };

    let new_model = match Text::new("修改默认模型 (回车保持原样):")
        .with_default(&current_model)
        .prompt()
    {
        Ok(m) => m.trim().to_string(),
        _ => String::new(),
    };

    let mut new_p = old_p.clone();
    new_p.name = new_name;

    match app_choice.as_str() {
        "claude" => {
            let kf = old_p
                .extract_api_key_field("claude")
                .unwrap_or_else(|| "ANTHROPIC_AUTH_TOKEN".to_string());
            let mut env = serde_json::json!({
                "ANTHROPIC_BASE_URL": new_url,
                kf.clone(): new_key,
            });
            if !new_model.is_empty() {
                if let Some(obj) = env.as_object_mut() {
                    obj.insert("ANTHROPIC_MODEL".into(), serde_json::Value::String(new_model));
                }
            }
            new_p.settings_config = serde_json::json!({ "env": env });
        }
        "codex" => {
            let wire = old_p.extract_wire_api("codex");
            let toml_config = format!(
                "model_provider = \"custom\"\nmodel = \"{new_model}\"\n\n[model_providers.custom]\nbase_url = \"{new_url}\"\nwire_api = \"{wire}\"\n"
            );
            new_p.settings_config = serde_json::json!({
                "auth": { "OPENAI_API_KEY": new_key },
                "config": toml_config
            });
        }
        "grok" => {
            let toml_config = format!("models_base_url = \"{new_url}\"\nmodel = \"{new_model}\"\n");
            new_p.settings_config = serde_json::json!({
                "auth": { "GROK_API_KEY": new_key },
                "config": toml_config
            });
        }
        _ => return,
    }

    match service.save_provider(&app_choice, new_p) {
        Ok(p) => {
            println!(
                "{} 成功修改供应商：{}",
                "✔".bright_green().bold(),
                p.name.bright_white().bold()
            );
        }
        Err(e) => {
            println!("{} 修改失败: {e}", "✖".bright_red().bold());
        }
    }
    println!();
}

pub async fn interactive_remove(service: &SwitchService) {
    let app_choice = match Select::new(
        "选择目标应用:",
        vec!["claude (Claude Code)", "codex (Codex)", "grok (Grok)"],
    )
    .prompt()
    {
        Ok(s) => s.split_whitespace().next().unwrap_or("claude").to_string(),
        Err(_) => return,
    };

    let root = service.get_root();
    let data = match root.apps.get(&app_choice) {
        Some(d) if !d.order.is_empty() => d,
        _ => {
            println!("{}", "该应用暂无可删除的供应商".bright_red());
            return;
        }
    };

    let mut provider_items = Vec::new();
    for id in &data.order {
        if let Some(p) = data.providers.get(id) {
            if store::is_official_provider(p) {
                continue; // 官方账号不可删除
            }
            provider_items.push(format!("{} ({})", p.name, p.id));
        }
    }

    if provider_items.is_empty() {
        println!("{}", "没有可删除的第三方供应商".bright_yellow());
        return;
    }

    let choice = match Select::new("选择要删除的供应商:", provider_items).prompt() {
        Ok(s) => s,
        Err(_) => return,
    };

    let id_part = choice.split('(').nth(1).unwrap_or("").trim_end_matches(')');

    let is_current = data.current.as_deref() == Some(id_part);
    let mode = if is_current {
        match Select::new(
            "该供应商正在使用中，删除后如何处理当前环境配置？",
            vec![
                "restore (安全恢复为官方默认基线)",
                "keep (保留当前已写入的配置内容)",
            ],
        )
        .prompt()
        {
            Ok(s) if s.starts_with("restore") => Some("restore"),
            Ok(_) => Some("keep"),
            Err(_) => return,
        }
    } else {
        None
    };

    let confirmed = Confirm::new("确认要永久删除该供应商吗？")
        .with_default(false)
        .prompt()
        .unwrap_or(false);

    if !confirmed {
        return;
    }

    match service.delete_provider(&app_choice, id_part, mode) {
        Ok(name) => {
            println!("{} 成功删除供应商：{}", "✔".bright_green(), name);
        }
        Err(e) => {
            println!("{} 删除失败: {e}", "✖".bright_red());
        }
    }
    println!();
}

pub async fn interactive_proxy_menu() {
    let port = DEFAULT_PORT;
    let running = daemon::is_running(port).await;

    println!();
    println!(
        "{} 本地代理状态: {}",
        "🌐".bright_cyan(),
        if running {
            format!("正在运行中 (127.0.0.1:{port})").bright_green().bold()
        } else {
            "未启动 (已停止)".dimmed()
        }
    );

    let options = if running {
        vec![
            "📊 查看当前代理路由与流量统计 (Status)",
            "🛑 停止后台代理 (Stop)",
            "🔄 重启后台代理 (Restart)",
            "📜 查看代理错误日志 (Logs)",
            "⬅ 返回上级菜单",
        ]
    } else {
        vec![
            "▶ 启动后台常驻代理 (Start)",
            "📜 查看代理历史错误日志 (Logs)",
            "⬅ 返回上级菜单",
        ]
    };

    let choice = match Select::new("选择代理管理操作:", options).prompt() {
        Ok(s) => s,
        Err(_) => return,
    };

    if choice.starts_with("▶") {
        let spinner = ProgressBar::new_spinner();
        spinner.set_message("正在拉起后台代理进程...");
        spinner.enable_steady_tick(Duration::from_millis(80));
        match daemon::start_background(port).await {
            Ok(_) => {
                spinner.finish_and_clear();
                println!(
                    "{} 后台代理已成功在 127.0.0.1:{} 启动常驻！",
                    "✔".bright_green().bold(),
                    port
                );
            }
            Err(e) => {
                spinner.finish_and_clear();
                println!("{} 启动代理失败: {e}", "✖".bright_red().bold());
            }
        }
    } else if choice.starts_with("🛑") {
        let spinner = ProgressBar::new_spinner();
        spinner.set_message("正在停止后台代理...");
        spinner.enable_steady_tick(Duration::from_millis(80));
        match daemon::stop(port).await {
            Ok(_) => {
                spinner.finish_and_clear();
                println!("{} 后台代理已安全退出。", "✔".bright_green().bold());
            }
            Err(e) => {
                spinner.finish_and_clear();
                println!("{} 停止代理异常: {e}", "✖".bright_red().bold());
            }
        }
    } else if choice.starts_with("🔄") {
        let _ = daemon::stop(port).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = daemon::start_background(port).await;
        println!("{} 后台代理已重启完成。", "✔".bright_green().bold());
    } else if choice.starts_with("📊") {
        match daemon::get_status(port).await {
            Ok(status) => {
                println!();
                println!("{}", "── 代理状态报告 ──".bright_cyan().bold());
                println!("  PID:           {}", status.pid.to_string().bright_yellow());
                println!("  端口:          {}", status.port.to_string().bright_yellow());
                println!("  已路由应用:    {:?}", status.routed_apps);
                for (app, url) in &status.targets {
                    println!("    • {}: {}", app.bright_green(), url.dimmed());
                }
                println!("  流量计数:");
                for (app, cnt) in &status.counters {
                    println!(
                        "    • {}: 活跃中 {} / 总请求数 {}",
                        app.bright_cyan(),
                        cnt.in_flight.to_string().bright_yellow(),
                        cnt.total.to_string().bright_white().bold()
                    );
                }
                println!();
            }
            Err(e) => println!("{} 获取状态失败: {e}", "✖".bright_red()),
        }
    } else if choice.starts_with("📜") {
        crate::proxy_log::interactive_view(20);
    }
}

pub async fn interactive_import(service: &SwitchService) {
    let choice = match Select::new(
        "选择导入来源:",
        vec![
            "📦 从 cc-switch 导入 (支持 SQLite 数据库 & JSON 备份)",
            "💻 从当前本机客户端环境导入 (~/.claude, ~/.codex, ~/.grok)",
            "⬅ 返回",
        ],
    )
    .prompt()
    {
        Ok(s) => s,
        Err(_) => return,
    };

    if choice.starts_with("📦") {
        match crate::ccswitch::import_auto(service) {
            Ok(count) => {
                println!(
                    "{} 成功从 cc-switch 导入 {} 个供应商！",
                    "✔".bright_green().bold(),
                    count.to_string().bright_yellow()
                );
            }
            Err(e) => println!("{} 导入失败: {e}", "✖".bright_red()),
        }
    } else if choice.starts_with("💻") {
        match service.import_live() {
            Ok(list) => {
                println!(
                    "{} 成功导入当前客户端配置：{:?}",
                    "✔".bright_green().bold(),
                    list
                );
            }
            Err(e) => println!("{} 导入失败: {e}", "✖".bright_red()),
        }
    }
    println!();
}

pub async fn interactive_doctor(service: &SwitchService) {
    println!();
    println!("{}", "🩺 正在执行环境自检与诊断...".bright_cyan().bold());
    let list = service.diagnose();

    for item in list {
        println!("{}", "─".repeat(60).dimmed());
        println!("应用: {}", item.app.bright_yellow().bold());
        println!(
            "当前供应商: {}",
            item.current_name.as_deref().unwrap_or("未设置")
        );
        println!(
            "Base URL:   {}",
            item.base_url.as_deref().unwrap_or("官方默认")
        );
        if item.healthy {
            println!("状态:       {}", "✔ 健康 (未发现占位残留)".bright_green());
        } else {
            println!(
                "状态:       {}",
                item.issue.as_deref().unwrap_or("异常").bright_red().bold()
            );
            println!(
                "  {} 提示：检测到本地代理占位残留，可执行 repair 一键修复",
                "💡".bright_yellow()
            );
        }
    }
    println!("{}", "─".repeat(60).dimmed());
    println!();
}

pub async fn interactive_settings(service: &SwitchService) {
    let root = service.get_root();
    let backup_on = root
        .settings
        .get("backupBeforeWrite")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let plugin_on = root
        .settings
        .get("applyClaudePlugin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let desktop_on = root
        .settings
        .get("applyClaudeDesktop")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let options = vec![
        format!(
            "{} 写入配置前自动备份 (backupBeforeWrite: {})",
            if backup_on { "✔" } else { "○" },
            backup_on
        ),
        format!(
            "{} 同步更新 VS Code Claude 插件 bypass (applyClaudePlugin: {})",
            if plugin_on { "✔" } else { "○" },
            plugin_on
        ),
        format!(
            "{} 同步更新 Claude Desktop 桌面端网关 (applyClaudeDesktop: {})",
            if desktop_on { "✔" } else { "○" },
            desktop_on
        ),
        "⬅ 返回".into(),
    ];

    let choice = match Select::new("选择要修改的设置项:", options).prompt() {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut new_root = root.clone();
    if choice.contains("backupBeforeWrite") {
        if let Some(s) = new_root.settings.as_object_mut() {
            s.insert("backupBeforeWrite".into(), serde_json::Value::Bool(!backup_on));
        }
    } else if choice.contains("applyClaudePlugin") {
        if let Some(s) = new_root.settings.as_object_mut() {
            s.insert("applyClaudePlugin".into(), serde_json::Value::Bool(!plugin_on));
        }
    } else if choice.contains("applyClaudeDesktop") {
        if let Some(s) = new_root.settings.as_object_mut() {
            s.insert("applyClaudeDesktop".into(), serde_json::Value::Bool(!desktop_on));
        }
    }

    if let Err(e) = store::save(&new_root) {
        println!("{} 保存设置失败: {e}", "✖".bright_red());
    } else {
        *service.root.lock().unwrap() = new_root;
        println!("{} 设置已更新！", "✔".bright_green());
    }
}
