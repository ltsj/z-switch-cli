//! z-switch-cli 主程序入口。
use clap::Parser;
use colored::Colorize;
use std::io::IsTerminal;

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

fn update_claude_model(env: &mut serde_json::Map<String, serde_json::Value>, model: &str) {
    let model_fields = [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
    ];
    let existing_field = model_fields
        .iter()
        .copied()
        .find(|field| env.contains_key(*field));
    match (existing_field, model.trim().is_empty()) {
        (Some(field), true) => {
            env.remove(field);
        }
        (Some(field), false) => {
            env.insert(
                field.to_string(),
                serde_json::Value::String(model.trim().to_string()),
            );
        }
        (None, false) => {
            env.insert(
                "ANTHROPIC_MODEL".into(),
                serde_json::Value::String(model.trim().to_string()),
            );
        }
        (None, true) => {}
    }
}

fn require_interactive_terminal() {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!(
            "当前命令需要交互式终端；脚本或管道调用请提供供应商参数，或使用对应的非交互子命令"
        );
        std::process::exit(2);
    }
}

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
            require_interactive_terminal();
            tui::run_interactive_menu(&service).await;
        }

        Some(Commands::List { target_app }) => {
            let root = service.get_root();
            let apps: Vec<&str> = match (target_app, target_app_default.as_deref()) {
                (Some(a), _) => vec![a.as_str()],
                (None, Some(a)) => vec![a],
                (None, None) => vec!["claude", "codex", "grok"],
            };
            println!();
            for app_name in apps {
                if let Some(data) = root.apps.get(app_name) {
                    let port =
                        daemon::preferred_port_for_app(app_name).unwrap_or(proxy::DEFAULT_PORT);
                    let proxy_alive = daemon::get_status(port).await.ok().is_some_and(|status| {
                        status.routed_apps.iter().any(|routed| routed == app_name)
                    });
                    tui::print_providers_table(app_name, data, proxy_alive);
                }
            }
        }

        Some(Commands::Use {
            query,
            target_app,
            proxy,
            direct,
            port,
        }) => {
            let selected_app = target_app
                .map(|a| a.as_str().to_string())
                .or_else(|| target_app_default.clone());
            let app_name = selected_app.clone().unwrap_or_else(|| "claude".to_string());

            let proxy_mode = if proxy {
                Some(true)
            } else if direct {
                Some(false)
            } else {
                None
            };

            if let Some(q) = query {
                match service
                    // 只把用户显式指定的端口传给 service。未指定端口时，
                    // service 需要根据当前 live 配置判断是否应从 GUI 的
                    // 8899 回退到 CLI 默认端口 8999。
                    .switch_async(&app_name, &q, proxy_mode, port)
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
                            let effective_port = daemon::preferred_port_for_app(&app_name)
                                .or(port)
                                .unwrap_or(proxy::DEFAULT_PORT);
                            println!(
                                "  {} 代理已在 127.0.0.1:{} 转发请求，无需重启终端即可生效！",
                                "ℹ".bright_blue(),
                                effective_port
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("{} 切换失败: {e}", "✖".bright_red().bold());
                        std::process::exit(1);
                    }
                }
            } else {
                require_interactive_terminal();
                tui::interactive_switch_with_options(
                    &service,
                    selected_app.as_deref(),
                    proxy_mode,
                    port,
                )
                .await;
            }
        }

        Some(Commands::Test {
            query,
            target_app,
            stream,
            all,
        }) => {
            let app_name = target_app
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
                let mut all_ok = true;
                for id in &data.order {
                    if let Some(p) = data.providers.get(id) {
                        all_ok &= tui::test_single_provider_with_stream(&app_name, p, stream).await;
                    }
                }
                if !all_ok {
                    std::process::exit(1);
                }
            } else if let Some(q) = query {
                if let Some((_, p)) = SwitchService::find_provider(data, &q) {
                    if !tui::test_single_provider_with_stream(&app_name, p, stream).await {
                        std::process::exit(1);
                    }
                } else {
                    eprintln!("{} 未找到供应商: {q}", "✖".bright_red());
                    std::process::exit(1);
                }
            } else {
                require_interactive_terminal();
                tui::interactive_test(&service).await;
            }
        }

        Some(Commands::Add {
            target_app,
            name,
            url,
            key,
            key_field,
            model,
            wire_api,
        }) => {
            let app_name = target_app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());

            let has_partial_cli_args = name.is_some()
                || url.is_some()
                || key.is_some()
                || key_field.is_some()
                || model.is_some()
                || wire_api.is_some();
            if let (Some(name), Some(url)) = (name, url) {
                let id = service::provider_id_from_name(&name);
                let key = key.unwrap_or_default();
                let model = model.unwrap_or_default();
                let wire = wire_api.unwrap_or_else(|| "responses".to_string());

                let provider = match app_name.as_str() {
                    "claude" => {
                        let kf = key_field.unwrap_or_else(|| "ANTHROPIC_AUTH_TOKEN".to_string());
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
                        let toml_config = store::build_codex_config(&name, &url, &model, &wire);
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
                            store::build_grok_config(&name, &url, &key, &model, &wire);
                        store::Provider {
                            id,
                            name,
                            category: Some("custom".into()),
                            settings_config: serde_json::json!({ "config": toml_config }),
                            meta: serde_json::json!({}),
                            failover: serde_json::json!({ "enabled": false }),
                        }
                    }
                    _ => {
                        eprintln!("{} 不支持的应用: {app_name}", "✖".bright_red());
                        std::process::exit(1);
                    }
                };

                match service.add_provider_async(&app_name, provider).await {
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
            } else if has_partial_cli_args {
                eprintln!(
                    "{} 非交互添加必须同时提供 --name 和 --url；未提供参数时才进入 TUI",
                    "✖".bright_red().bold()
                );
                std::process::exit(2);
            } else {
                require_interactive_terminal();
                tui::interactive_add(&service).await;
            }
        }

        Some(Commands::Edit {
            query,
            target_app,
            name,
            url,
            key,
            model,
            port,
        }) => {
            let app_name = target_app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());

            let has_edit_args = name.is_some() || url.is_some() || key.is_some() || model.is_some();
            if let Some(q) = query {
                let root = service.get_root();
                let data = match root.apps.get(&app_name) {
                    Some(d) => d,
                    None => {
                        eprintln!("{} 未知应用: {app_name}", "✖".bright_red());
                        std::process::exit(1);
                    }
                };
                if let Some((provider_id, old_p)) = SwitchService::find_provider(data, &q) {
                    if store::is_official_provider_for_app(&app_name, old_p) {
                        eprintln!("{} 官方基线供应商不可修改", "✖".bright_red());
                        std::process::exit(1);
                    }
                    let mut new_p = old_p.clone();
                    new_p.id = provider_id.clone();
                    if let Some(n) = name {
                        if n.trim().is_empty() {
                            eprintln!("{} 供应商名称不能为空", "✖".bright_red());
                            std::process::exit(2);
                        }
                        new_p.name = n.trim().to_string();
                    }
                    // 兼容旧版 GUI JSON 编辑产生的空名称卡片。Codex
                    // 要求 model provider 的 name 非空，编辑模型时顺手修复。
                    new_p.name = store::provider_name_for_edit(&new_p.name, provider_id);
                    let model_was_supplied = model.is_some();
                    let target_url = url
                        .or_else(|| old_p.extract_base_url(&app_name))
                        .unwrap_or_default();
                    let target_key = key
                        .or_else(|| old_p.extract_api_key(&app_name))
                        .unwrap_or_default();
                    let target_model = model
                        .or_else(|| old_p.extract_model(&app_name))
                        .unwrap_or_default();

                    match app_name.as_str() {
                        "claude" => {
                            let kf = old_p
                                .extract_api_key_field("claude")
                                .unwrap_or_else(|| "ANTHROPIC_AUTH_TOKEN".to_string());
                            let mut env = old_p
                                .settings_config
                                .get("env")
                                .and_then(serde_json::Value::as_object)
                                .cloned()
                                .unwrap_or_default();
                            env.insert(
                                "ANTHROPIC_BASE_URL".into(),
                                serde_json::Value::String(target_url),
                            );
                            env.insert(kf, serde_json::Value::String(target_key));
                            if model_was_supplied {
                                update_claude_model(&mut env, &target_model);
                            }
                            new_p.settings_config =
                                serde_json::json!({ "env": serde_json::Value::Object(env) });
                        }
                        "codex" => {
                            let wire = old_p.extract_wire_api("codex");
                            let toml_config = old_p
                                .settings_config
                                .get("config")
                                .and_then(serde_json::Value::as_str)
                                .map(|existing| {
                                    store::build_codex_config_preserving(
                                        existing,
                                        &new_p.name,
                                        &target_url,
                                        &target_model,
                                        &wire,
                                    )
                                })
                                .unwrap_or_else(|| {
                                    store::build_codex_config(
                                        &new_p.name,
                                        &target_url,
                                        &target_model,
                                        &wire,
                                    )
                                });
                            let mut auth = old_p
                                .settings_config
                                .get("auth")
                                .and_then(serde_json::Value::as_object)
                                .cloned()
                                .unwrap_or_default();
                            auth.insert(
                                "OPENAI_API_KEY".into(),
                                serde_json::Value::String(target_key),
                            );
                            new_p.settings_config = serde_json::json!({
                                "auth": serde_json::Value::Object(auth),
                                "config": toml_config
                            });
                        }
                        "grok" => {
                            let wire = old_p.extract_wire_api("grok");
                            let toml_config = old_p
                                .settings_config
                                .get("config")
                                .and_then(serde_json::Value::as_str)
                                .map(|existing| {
                                    store::build_grok_config_preserving(
                                        existing,
                                        &new_p.name,
                                        &target_url,
                                        &target_key,
                                        &target_model,
                                        &wire,
                                    )
                                })
                                .unwrap_or_else(|| {
                                    store::build_grok_config(
                                        &new_p.name,
                                        &target_url,
                                        &target_key,
                                        &target_model,
                                        &wire,
                                    )
                                });
                            new_p.settings_config = serde_json::json!({
                                "config": toml_config
                            });
                        }
                        _ => {
                            eprintln!("{} 不支持的应用: {app_name}", "✖".bright_red());
                            std::process::exit(1);
                        }
                    }

                    match service.save_provider_async(&app_name, new_p, port).await {
                        Ok(p) => println!(
                            "{} 成功修改供应商：{}",
                            "✔".bright_green().bold(),
                            p.name.bright_white().bold()
                        ),
                        Err(e) => {
                            eprintln!("{} 修改失败: {e}", "✖".bright_red().bold());
                            std::process::exit(1);
                        }
                    }
                } else {
                    eprintln!("{} 未找到供应商: {q}", "✖".bright_red());
                    std::process::exit(1);
                }
            } else if has_edit_args || port.is_some() {
                eprintln!(
                    "{} 编辑命令带字段参数时必须提供供应商名称或 ID",
                    "✖".bright_red().bold()
                );
                std::process::exit(2);
            } else {
                require_interactive_terminal();
                tui::interactive_edit(&service).await;
            }
        }

        Some(Commands::Remove {
            query,
            target_app,
            mode,
            port,
        }) => {
            let app_name = target_app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());

            if let Some(q) = query {
                match service
                    .delete_provider(&app_name, &q, mode.as_deref(), port)
                    .await
                {
                    Ok(name) => {
                        println!("{} 成功删除供应商：{}", "✔".bright_green(), name);
                    }
                    Err(e) => {
                        eprintln!("{} 删除失败: {e}", "✖".bright_red());
                        std::process::exit(1);
                    }
                }
            } else {
                require_interactive_terminal();
                tui::interactive_remove(&service).await;
            }
        }

        Some(Commands::Proxy { action }) => match action {
            ProxyAction::Start { port, foreground } => {
                if foreground {
                    println!("正在前台启动代理服务 (127.0.0.1:{port})... 按 Ctrl+C 退出");
                    if let Err(e) = daemon::run_worker(port).await {
                        eprintln!("{} 代理服务异常退出: {e}", "✖".bright_red());
                        std::process::exit(1);
                    }
                } else {
                    println!("正在启动后台代理守护进程 (127.0.0.1:{port})...");
                    match daemon::start_background(port).await {
                        Ok(_) => println!(
                            "{} 后台常驻代理已成功在 127.0.0.1:{} 启动运行！",
                            "✔".bright_green().bold(),
                            port
                        ),
                        Err(e) => {
                            eprintln!("{} 启动失败: {e}", "✖".bright_red().bold());
                            std::process::exit(1);
                        }
                    }
                }
            }

            ProxyAction::Stop { port } => match service.stop_proxy(port).await {
                Ok(_) => println!("{} 后台代理服务已安全退出。", "✔".bright_green().bold()),
                Err(e) => {
                    eprintln!("{} 停止代理异常: {e}", "✖".bright_red());
                    std::process::exit(1);
                }
            },

            ProxyAction::Restart { port } => match service.restart_proxy(port).await {
                Ok(_) => println!("{} 后台代理重启成功！", "✔".bright_green().bold()),
                Err(e) => {
                    eprintln!("{} 重启失败: {e}", "✖".bright_red().bold());
                    std::process::exit(1);
                }
            },

            ProxyAction::Status { port } => match daemon::get_status(port).await {
                Ok(status) => {
                    println!();
                    println!("{}", "── 代理状态报告 ──".bright_cyan().bold());
                    println!(
                        "  PID:           {}",
                        status.pid.to_string().bright_yellow()
                    );
                    println!(
                        "  端口:          {}",
                        status.port.to_string().bright_yellow()
                    );
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
                Err(e) => {
                    eprintln!("{} 代理未运行或未响应: {e}", "○".dimmed());
                    std::process::exit(1);
                }
            },

            ProxyAction::Logs { tail } => {
                proxy_log::interactive_view(tail);
            }

            ProxyAction::Worker { .. } => unreachable!(),
        },

        Some(Commands::Import { source }) => match source {
            ImportSource::CcSwitch { db, json } => {
                if let Some(db_path) = db {
                    match ccswitch::import_from_sqlite_path(
                        &service,
                        std::path::Path::new(&db_path),
                    ) {
                        Ok(n) => println!("{} 成功导入 {n} 个供应商！", "✔".bright_green().bold()),
                        Err(e) => {
                            eprintln!("{} 导入失败: {e}", "✖".bright_red());
                            std::process::exit(1);
                        }
                    }
                } else if let Some(json_path) = json {
                    match ccswitch::import_from_json_path(
                        &service,
                        std::path::Path::new(&json_path),
                    ) {
                        Ok(n) => println!("{} 成功导入 {n} 个供应商！", "✔".bright_green().bold()),
                        Err(e) => {
                            eprintln!("{} 导入失败: {e}", "✖".bright_red());
                            std::process::exit(1);
                        }
                    }
                } else {
                    match ccswitch::import_auto(&service) {
                        Ok(n) => println!("{} 成功导入 {n} 个供应商！", "✔".bright_green().bold()),
                        Err(e) => {
                            eprintln!("{} 导入失败: {e}", "✖".bright_red());
                            std::process::exit(1);
                        }
                    }
                }
            }
            ImportSource::Live => match service.import_live() {
                Ok(list) => {
                    println!(
                        "{} 成功从本地客户端导入配置：{:?}",
                        "✔".bright_green().bold(),
                        list
                    )
                }
                Err(e) => {
                    eprintln!("{} 导入失败: {e}", "✖".bright_red());
                    std::process::exit(1);
                }
            },
        },

        Some(Commands::Export { output }) => {
            let root = service.get_root();
            let json_str = serde_json::to_string_pretty(&root).unwrap();
            if let Some(out_path) = output {
                if let Err(e) =
                    config::atomic_write(std::path::Path::new(&out_path), json_str.as_bytes())
                {
                    eprintln!("{} 导出文件写入失败: {e}", "✖".bright_red());
                    std::process::exit(1);
                } else {
                    println!(
                        "{} 配置已成功导出至 {}",
                        "✔".bright_green().bold(),
                        out_path
                    );
                }
            } else {
                println!("{json_str}");
            }
        }

        Some(Commands::Restore { target_app, port }) => {
            let app_name = target_app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());
            match service.restore_official_baseline(&app_name, port).await {
                Ok(_) => println!(
                    "{} 已成功将 {} 恢复为官方初始基线！",
                    "✔".bright_green().bold(),
                    app_name
                ),
                Err(e) => {
                    eprintln!("{} 恢复失败: {e}", "✖".bright_red());
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Doctor) => {
            if !tui::interactive_doctor(&service).await {
                std::process::exit(1);
            }
        }

        Some(Commands::Repair { target_app, port }) => {
            let app_name = target_app
                .map(|a| a.as_str().to_string())
                .or(target_app_default)
                .unwrap_or_else(|| "claude".to_string());
            match service.repair_app(&app_name, port).await {
                Ok(_) => println!("{} 修复完成！", "✔".bright_green().bold()),
                Err(e) => {
                    eprintln!("{} 修复失败: {e}", "✖".bright_red());
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Config { key, value }) => {
            if let Some(k) = key {
                if let Some(v) = value {
                    match v.parse::<bool>() {
                        Ok(b) => match service.set_bool_setting(&k, b) {
                            Ok(()) => {
                                println!("{} 配置项 {} 已更新为 {}", "✔".bright_green(), k, b)
                            }
                            Err(e) => {
                                eprintln!("{} 保存失败: {e}", "✖".bright_red());
                                std::process::exit(1);
                            }
                        },
                        Err(_) => {
                            eprintln!("{} 配置值必须是 true 或 false", "✖".bright_red());
                            std::process::exit(1);
                        }
                    }
                } else {
                    let root = service.get_root();
                    let cur = root.settings.get(&k);
                    println!("{} = {:?}", k, cur);
                }
            } else {
                let root = service.get_root();
                println!("{}", serde_json::to_string_pretty(&root.settings).unwrap());
            }
        }
    }
}
