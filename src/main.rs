//! z-switch-cli 主程序入口。
use clap::Parser;
use colored::Colorize;

mod ccswitch;
mod claude_desktop;
mod claude_ext;
mod cli;
mod config;
mod connectivity;
mod daemon;
mod live;
mod model_fetch;
mod official;
mod original;
mod proxy;
mod proxy_log;
mod repair;
mod service;
mod store;
mod stream_test;
mod tui;

use cli::{Cli, Commands, ImportSource, ProxyAction};
use service::SwitchService;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // 如果是内部守护工作进程，直接执行 worker 循环（不初始化 service 避免开销）
    if let Some(Commands::Proxy {
        action: ProxyAction::Worker { port },
    }) = cli.command
    {
        if let Err(e) = daemon::run_worker(port).await {
            eprintln!("[z-switch-worker] 异常退出: {e}");
            std::process::exit(1);
        }
        return;
    }

    let service = SwitchService::new();

    let target_app_default = cli.app.map(|a| a.as_str().to_string());

    match cli.command {
        None => {
            tui::run_interactive_menu(&service).await;
        }

        Some(Commands::List { app }) => {
            let root = service.get_root();
            let proxy_alive = daemon::is_running(proxy::DEFAULT_PORT).await;
            let apps: Vec<&str> = match (app, target_app_default.as_deref()) {
                (Some(a), _) => vec![a.as_str()],
                (None, Some(a)) => vec![a],
                (None, None) => vec!["claude", "codex", "grok"],
            };
            println!();
            for app_name in apps {
                if let Some(data) = root.apps.get(app_name) {
                    tui::print_providers_table(app_name, data, proxy_alive);
                }
            }
        }

        Some(Commands::Use {
            query,
            app,
            proxy,
            direct,
            port,
        }) => {
            let app_name = app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());

            let proxy_mode = if proxy {
                Some(true)
            } else if direct {
                Some(false)
            } else {
                None
            };

            let port = port.unwrap_or(proxy::DEFAULT_PORT);

            if let Some(q) = query {
                match service
                    .switch_async(&app_name, &q, proxy_mode, Some(port))
                    .await
                {
                    Ok((p, is_proxied)) => {
                        let mode_str = if is_proxied {
                            "【本地代理热切模式】".bright_green().bold()
                        } else {
                            "【直连模式】".bright_cyan().bold()
                        };
                        println!(
                            "{} 已成功将 {} 切换至供应商：{} {}",
                            "✔".bright_green().bold(),
                            app_name.bright_yellow(),
                            p.name.bright_white().bold(),
                            mode_str
                        );
                        if is_proxied {
                            println!(
                                "  {} 代理已在 127.0.0.1:{} 转发请求，无需重启终端即可生效！",
                                "ℹ".bright_blue(),
                                port
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("{} 切换失败: {e}", "✖".bright_red().bold());
                        std::process::exit(1);
                    }
                }
            } else {
                tui::interactive_switch(&service).await;
            }
        }

        Some(Commands::Test {
            query,
            app,
            stream: _,
            all,
        }) => {
            let app_name = app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());

            let root = service.get_root();
            let data = match root.apps.get(&app_name) {
                Some(d) => d,
                None => {
                    eprintln!("{} 未知应用: {app_name}", "✖".bright_red());
                    std::process::exit(1);
                }
            };

            if all {
                for id in &data.order {
                    if let Some(p) = data.providers.get(id) {
                        tui::test_single_provider(&app_name, p).await;
                    }
                }
            } else if let Some(q) = query {
                if let Some((_, p)) = SwitchService::find_provider(data, &q) {
                    tui::test_single_provider(&app_name, p).await;
                } else {
                    eprintln!("{} 未找到供应商: {q}", "✖".bright_red());
                    std::process::exit(1);
                }
            } else {
                tui::interactive_test(&service).await;
            }
        }

        Some(Commands::Add {
            app,
            name,
            url,
            key,
            key_field,
            model,
            wire_api,
        }) => {
            let app_name = app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());

            if let (Some(name), Some(url)) = (name, url) {
                let id = name
                    .to_lowercase()
                    .replace(|c: char| !c.is_alphanumeric(), "-");
                let key = key.unwrap_or_default();
                let model = model.unwrap_or_default();
                let wire = wire_api.unwrap_or_else(|| "responses".to_string());

                let provider = match app_name.as_str() {
                    "claude" => {
                        let kf = key_field
                            .unwrap_or_else(|| "ANTHROPIC_AUTH_TOKEN".to_string());
                        let mut env = serde_json::json!({
                            "ANTHROPIC_BASE_URL": url,
                            kf.clone(): key,
                        });
                        if !model.is_empty() {
                            if let Some(obj) = env.as_object_mut() {
                                obj.insert(
                                    "ANTHROPIC_MODEL".into(),
                                    serde_json::Value::String(model),
                                );
                            }
                        }
                        store::Provider {
                            id,
                            name,
                            category: Some("custom".into()),
                            settings_config: serde_json::json!({ "env": env }),
                            meta: serde_json::json!({ "apiKeyField": kf }),
                            failover: serde_json::json!({ "enabled": false }),
                        }
                    }
                    "codex" => {
                        let toml_config = format!(
                            "model_provider = \"custom\"\nmodel = \"{model}\"\n\n[model_providers.custom]\nbase_url = \"{url}\"\nwire_api = \"{wire}\"\n"
                        );
                        store::Provider {
                            id,
                            name,
                            category: Some("custom".into()),
                            settings_config: serde_json::json!({
                                "auth": { "OPENAI_API_KEY": key },
                                "config": toml_config
                            }),
                            meta: serde_json::json!({ "wireApi": wire }),
                            failover: serde_json::json!({ "enabled": false }),
                        }
                    }
                    "grok" => {
                        let toml_config =
                            format!("models_base_url = \"{url}\"\nmodel = \"{model}\"\n");
                        store::Provider {
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
                    _ => {
                        eprintln!("{} 不支持的应用: {app_name}", "✖".bright_red());
                        std::process::exit(1);
                    }
                };

                match service.save_provider(&app_name, provider) {
                    Ok(p) => println!(
                        "{} 成功添加供应商：{}",
                        "✔".bright_green().bold(),
                        p.name.bright_white().bold()
                    ),
                    Err(e) => {
                        eprintln!("{} 添加失败: {e}", "✖".bright_red().bold());
                        std::process::exit(1);
                    }
                }
            } else {
                tui::interactive_add(&service).await;
            }
        }

        Some(Commands::Remove { query, app, mode }) => {
            let app_name = app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());

            if let Some(q) = query {
                match service.delete_provider(&app_name, &q, mode.as_deref()) {
                    Ok(name) => {
                        println!("{} 成功删除供应商：{}", "✔".bright_green(), name);
                    }
                    Err(e) => {
                        eprintln!("{} 删除失败: {e}", "✖".bright_red());
                        std::process::exit(1);
                    }
                }
            } else {
                tui::interactive_remove(&service).await;
            }
        }

        Some(Commands::Proxy { action }) => match action {
            ProxyAction::Start { port, foreground } => {
                if foreground {
                    println!("正在前台启动代理服务 (127.0.0.1:{port})... 按 Ctrl+C 退出");
                    if let Err(e) = daemon::run_worker(port).await {
                        eprintln!("{} 代理服务异常退出: {e}", "✖".bright_red());
                    }
                } else {
                    println!("正在启动后台代理守护进程 (127.0.0.1:{port})...");
                    match daemon::start_background(port).await {
                        Ok(_) => println!(
                            "{} 后台常驻代理已成功在 127.0.0.1:{} 启动运行！",
                            "✔".bright_green().bold(),
                            port
                        ),
                        Err(e) => eprintln!("{} 启动失败: {e}", "✖".bright_red().bold()),
                    }
                }
            }

            ProxyAction::Stop { port } => match daemon::stop(port).await {
                Ok(_) => println!("{} 后台代理服务已安全退出。", "✔".bright_green().bold()),
                Err(e) => eprintln!("{} 停止代理异常: {e}", "✖".bright_red()),
            },

            ProxyAction::Restart { port } => {
                let _ = daemon::stop(port).await;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                match daemon::start_background(port).await {
                    Ok(_) => println!("{} 后台代理重启成功！", "✔".bright_green().bold()),
                    Err(e) => eprintln!("{} 重启失败: {e}", "✖".bright_red().bold()),
                }
            }

            ProxyAction::Status { port } => match daemon::get_status(port).await {
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
                Err(e) => println!("{} 代理未运行或未响应: {e}", "○".dimmed()),
            },

            ProxyAction::Logs { tail } => {
                proxy_log::interactive_view(tail);
            }

            ProxyAction::Worker { .. } => unreachable!(),
        },

        Some(Commands::Import { source }) => match source {
            ImportSource::CcSwitch { db, json } => {
                if let Some(db_path) = db {
                    match ccswitch::import_from_sqlite_path(&service, std::path::Path::new(&db_path)) {
                        Ok(n) => println!("{} 成功导入 {n} 个供应商！", "✔".bright_green().bold()),
                        Err(e) => eprintln!("{} 导入失败: {e}", "✖".bright_red()),
                    }
                } else if let Some(json_path) = json {
                    match ccswitch::import_from_json_path(&service, std::path::Path::new(&json_path)) {
                        Ok(n) => println!("{} 成功导入 {n} 个供应商！", "✔".bright_green().bold()),
                        Err(e) => eprintln!("{} 导入失败: {e}", "✖".bright_red()),
                    }
                } else {
                    match ccswitch::import_auto(&service) {
                        Ok(n) => println!("{} 成功导入 {n} 个供应商！", "✔".bright_green().bold()),
                        Err(e) => eprintln!("{} 导入失败: {e}", "✖".bright_red()),
                    }
                }
            }
            ImportSource::Live => match service.import_live() {
                Ok(list) => {
                    println!("{} 成功从本地客户端导入配置：{:?}", "✔".bright_green().bold(), list)
                }
                Err(e) => eprintln!("{} 导入失败: {e}", "✖".bright_red()),
            },
        },

        Some(Commands::Export { output }) => {
            let root = service.get_root();
            let json_str = serde_json::to_string_pretty(&root).unwrap();
            if let Some(out_path) = output {
                if let Err(e) = config::atomic_write(std::path::Path::new(&out_path), json_str.as_bytes()) {
                    eprintln!("{} 导出文件写入失败: {e}", "✖".bright_red());
                } else {
                    println!("{} 配置已成功导出至 {}", "✔".bright_green().bold(), out_path);
                }
            } else {
                println!("{json_str}");
            }
        }

        Some(Commands::Restore { app }) => {
            let app_name = app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());
            match service.restore_official_baseline(&app_name) {
                Ok(_) => println!(
                    "{} 已成功将 {} 恢复为官方初始基线！",
                    "✔".bright_green().bold(),
                    app_name
                ),
                Err(e) => eprintln!("{} 恢复失败: {e}", "✖".bright_red()),
            }
        }

        Some(Commands::Doctor) => {
            tui::interactive_doctor(&service).await;
        }

        Some(Commands::Repair { app }) => {
            let app_name = app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());
            match service.repair_app(&app_name) {
                Ok(_) => println!("{} 修复完成！", "✔".bright_green().bold()),
                Err(e) => eprintln!("{} 修复失败: {e}", "✖".bright_red()),
            }
        }

        Some(Commands::Config { key, value }) => {
            let mut root = service.get_root();
            if let Some(k) = key {
                if let Some(v) = value {
                    let b = v.parse::<bool>().unwrap_or(false);
                    if let Some(s) = root.settings.as_object_mut() {
                        s.insert(k.clone(), serde_json::Value::Bool(b));
                    }
                    if let Err(e) = store::save(&root) {
                        eprintln!("{} 保存失败: {e}", "✖".bright_red());
                    } else {
                        println!("{} 配置项 {} 已更新为 {}", "✔".bright_green(), k, b);
                    }
                } else {
                    let cur = root.settings.get(&k);
                    println!("{} = {:?}", k, cur);
                }
            } else {
                println!("{}", serde_json::to_string_pretty(&root.settings).unwrap());
            }
        }
    }
}
