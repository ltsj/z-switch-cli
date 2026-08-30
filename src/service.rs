//! 核心业务服务层：串联配置读写、切换、测速、生态同步与自愈。
use serde_json::Value;
use std::sync::Mutex;

use crate::claude_desktop;
use crate::claude_ext;
use crate::daemon;
use crate::live;
use crate::official;
use crate::original;
use crate::proxy;
use crate::repair;
use crate::store::{self, Provider, Root};

const BOOL_SETTING_KEYS: &[&str] = &[
    "backupBeforeWrite",
    "applyClaudePlugin",
    "skipClaudeOnboarding",
    "applyClaudeDesktop",
];

fn active_foreign_proxy_error(app: &str, port: u16, foreign_port: u16) -> String {
    format!(
        "{app} 当前由其它本地代理 127.0.0.1:{foreign_port} 接管，未覆盖 live 配置；请先停止该代理，或明确使用 --proxy --port {port} 接管"
    )
}

fn validate_provider(app: &str, provider: &Provider) -> Result<(), String> {
    if !proxy::PROXY_APPS.contains(&app) {
        return Err(format!("未知应用: {app}"));
    }
    if store::is_official_provider_for_app(app, provider) {
        return Ok(());
    }
    if provider.name.trim().is_empty() {
        return Err("供应商名称不能为空".into());
    }
    if provider_has_proxy_placeholder(app, provider) {
        return Err("供应商配置包含本地代理占位密钥，请重新填写真实 API Key".into());
    }
    if app == "claude" {
        let key_field = provider
            .meta
            .get("apiKeyField")
            .and_then(Value::as_str)
            .unwrap_or("ANTHROPIC_AUTH_TOKEN");
        if !matches!(key_field, "ANTHROPIC_AUTH_TOKEN" | "ANTHROPIC_API_KEY") {
            return Err("Claude Key 字段只能是 ANTHROPIC_AUTH_TOKEN 或 ANTHROPIC_API_KEY".into());
        }
    }
    let base_url = provider
        .extract_base_url(app)
        .ok_or_else(|| format!("供应商 {} 缺少 Base URL", provider.name))?;
    proxy::validate_base_url(&base_url)
}

fn provider_has_proxy_placeholder(app: &str, provider: &Provider) -> bool {
    match app {
        "claude" => provider
            .settings_config
            .get("env")
            .and_then(Value::as_object)
            .is_some_and(|env| {
                ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
                    .iter()
                    .filter_map(|key| env.get(*key).and_then(Value::as_str))
                    .any(proxy::is_placeholder_key)
            }),
        "codex" => provider
            .settings_config
            .get("auth")
            .and_then(|auth| auth.get("OPENAI_API_KEY"))
            .and_then(Value::as_str)
            .is_some_and(proxy::is_placeholder_key),
        "grok" => {
            let auth_placeholder = provider
                .settings_config
                .get("auth")
                .and_then(Value::as_object)
                .is_some_and(|auth| {
                    ["GROK_API_KEY", "XAI_API_KEY"]
                        .iter()
                        .filter_map(|key| auth.get(*key).and_then(Value::as_str))
                        .any(proxy::is_placeholder_key)
                });
            let config_placeholder = provider
                .settings_config
                .get("config")
                .and_then(Value::as_str)
                .is_some_and(|config| {
                    ["api_key", "grok_api_key", "xai_api_key"]
                        .iter()
                        .any(|key| {
                            store::extract_grok_endpoint_string(config, key)
                                .or_else(|| store::extract_grok_model_string(config, key))
                                .is_some_and(|value| proxy::is_placeholder_key(&value))
                        })
                });
            auth_placeholder || config_placeholder
        }
        _ => false,
    }
}

fn validate_wire_api(app: &str, provider: &Provider) -> Result<(), String> {
    if app != "claude"
        && !matches!(
            provider.extract_wire_api(app).as_str(),
            "chat" | "responses"
        )
    {
        return Err("wire_api 只能是 chat 或 responses".into());
    }
    Ok(())
}

pub struct SwitchService {
    pub root: Mutex<Root>,
}

impl SwitchService {
    pub fn new() -> Self {
        if let Err(e) = official::capture_codex_if_logged_in() {
            eprintln!("[z-switch] 初始化 Codex 官方登录态警告: {e}");
        }

        let snapshot_ready = match original::capture_once() {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[z-switch] 保存本机原始配置警告: {e}");
                false
            }
        };

        if let Err(e) = original::capture_grok_if_missing() {
            eprintln!("[z-switch] 补采 Grok 原始配置警告: {e}");
        }

        // 启动迁移必须持有 StoreSession 直到提交完成，否则 GUI 可能在
        // load_checked() 与 save() 之间写入新配置，随后又被 CLI 的旧快照覆盖。
        let mut store_session = match store::StoreSession::begin() {
            Ok(session) => Some(session),
            Err(error) => {
                eprintln!("[z-switch] 共享配置不可读取，已进入只读保护模式: {error}");
                None
            }
        };
        let mut root = store_session
            .as_ref()
            .map(|session| session.root.clone())
            .unwrap_or_else(Root::default_seeded);
        let mut root_changed = store_session
            .as_ref()
            .is_some_and(store::StoreSession::has_changes);
        root_changed |= root.ensure_official_providers();

        let initial_import_done = root
            .settings
            .get("initialImportDone")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if snapshot_ready && !initial_import_done {
            let already_has_provider = root.has_non_official_provider();
            let imported = if already_has_provider {
                false
            } else {
                import_live_in_place(&mut root)
            };
            // 代理占用时 live 导入会主动跳过，不能把“尚未完成”记成
            // true；等代理解除后下一次启动才有机会导入真实上游配置。
            if already_has_provider || imported {
                if let Some(settings) = root.settings.as_object_mut() {
                    settings.insert("initialImportDone".into(), Value::Bool(true));
                }
                root_changed = true;
            }
        }

        // Grok 是后加入的客户端。老用户可能已经完成全局首次导入，
        // 但 providers.json 里仍没有 Grok 卡片；这里单独对齐原 GUI 的
        // 一次性采纳逻辑。若 live 当前仍被本地代理占用，import_grok()
        // 会返回 None，但不能因此把标记写成 true，否则真实上游永远不会
        // 在代理退出后被采纳。
        let grok_import_done = root
            .settings
            .get("grokImportDone")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if snapshot_ready && !grok_import_done {
            let grok_empty = root
                .apps
                .get("grok")
                .map(|data| data.providers.is_empty())
                .unwrap_or(true);
            let grok_live = repair::read_grok();
            let grok_proxy_active =
                live::proxy_port("grok").is_some() || grok_live.key_is_placeholder;

            if grok_empty && !grok_proxy_active {
                if let Some(mut provider) = live::import_grok() {
                    let data = root.apps.entry("grok".into()).or_default();
                    let id = unique_id(&data.providers, "imported-current");
                    provider.id = id.clone();
                    if !data.order.contains(&id) {
                        data.order.push(id.clone());
                    }
                    data.providers.insert(id.clone(), provider);
                    data.current = Some(id);
                }
            }

            // 没有配置、配置已被采纳，或配置格式无效时都可以结束一次性
            // 检查；只有代理占用这一种情况必须保留 false 以便重试。
            if !grok_empty || !grok_proxy_active {
                if let Some(settings) = root.settings.as_object_mut() {
                    settings.insert("grokImportDone".into(), Value::Bool(true));
                }
                root_changed = true;
            }
        }

        for app in ["claude", "codex"] {
            if let Some(id) = store::official_provider_id(app) {
                if let Some(provider) = root
                    .apps
                    .get_mut(app)
                    .and_then(|data| data.providers.get_mut(id))
                {
                    root_changed |= live::hydrate_official_provider(app, provider);
                }
            }
        }

        if root_changed {
            if let Some(mut session) = store_session.take() {
                session.root = root.clone();
                if let Err(error) = session.commit() {
                    eprintln!("[z-switch] 保存共享配置迁移结果失败: {error}");
                }
            } else {
                eprintln!("[z-switch] 共享配置处于只读保护模式，跳过迁移写入");
            }
        } else {
            // 显式释放锁，避免 service 初始化完成后仍阻塞 GUI 的写入。
            drop(store_session);
        }

        Self {
            root: Mutex::new(root),
        }
    }

    pub fn get_root(&self) -> Root {
        // TUI 可能长时间驻留，GUI 进程也可能同时修改共享文件；读取展示数据
        // 时重新从磁盘载入，并在同一把锁内完成必要的结构迁移。
        let root = match store::StoreSession::begin() {
            Ok(session) => match session.commit_if_changed() {
                Ok(root) => root,
                Err(error) => {
                    eprintln!("[z-switch] 补齐共享配置结构失败: {error}");
                    self.root.lock().unwrap().clone()
                }
            },
            Err(error) => {
                eprintln!("[z-switch] 共享配置不可读取，展示最近可用快照: {error}");
                self.root.lock().unwrap().clone()
            }
        };
        *self.root.lock().unwrap() = root.clone();
        root
    }

    fn commit_session(&self, session: store::StoreSession) -> Result<(), String> {
        let root = session.commit()?;
        *self.root.lock().unwrap() = root;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn backup_flag(&self) -> bool {
        let root = self.root.lock().unwrap();
        root.settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    pub fn set_bool_setting(&self, key: &str, value: bool) -> Result<(), String> {
        let key = key.trim();
        if !BOOL_SETTING_KEYS.contains(&key) {
            return Err(format!(
                "不支持的布尔配置项: {key}（可选: {}）",
                BOOL_SETTING_KEYS.join(" / ")
            ));
        }
        let mut session = store::StoreSession::begin()?;
        if !session.root.settings.is_object() {
            return Err("系统设置不是 JSON 对象".to_string());
        }
        let previous = session
            .root
            .settings
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let side_effect_root = session.root.clone();
        apply_bool_setting_side_effect(&side_effect_root, key, value)?;
        let settings = session
            .root
            .settings
            .as_object_mut()
            .expect("settings object checked above");
        settings.insert(key.to_string(), Value::Bool(value));
        if let Err(error) = self.commit_session(session) {
            if let Err(rollback_error) =
                apply_bool_setting_side_effect(&side_effect_root, key, previous)
            {
                eprintln!("[z-switch] 回滚设置副作用失败: {rollback_error}");
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn find_provider<'a>(
        data: &'a store::AppData,
        query: &str,
    ) -> Option<(&'a String, &'a Provider)> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        if let Some(entry) = data.providers.get_key_value(query) {
            return Some(entry);
        }
        let q_lower = query.to_lowercase();
        let mut ordered_ids: Vec<&String> = data
            .order
            .iter()
            .filter(|id| data.providers.contains_key(*id))
            .collect();
        let mut missing_ids: Vec<&String> = data
            .providers
            .keys()
            .filter(|id| !data.order.contains(id))
            .collect();
        missing_ids.sort();
        ordered_ids.extend(missing_ids);

        // 精确匹配名称
        for id in &ordered_ids {
            let provider = &data.providers[*id];
            if provider.name.to_lowercase() == q_lower {
                return Some((*id, provider));
            }
        }

        // 包含匹配按用户可见顺序选择，避免 HashMap 随机迭代导致结果漂移。
        for id in ordered_ids {
            let provider = &data.providers[id];
            if provider.name.to_lowercase().contains(&q_lower)
                || id.to_lowercase().contains(&q_lower)
            {
                return Some((id, provider));
            }
        }
        None
    }

    /// 切换供应商（同步直连/代理）
    #[allow(dead_code)]
    pub fn switch(
        &self,
        app: &str,
        query: &str,
        proxy_handle: Option<&proxy::ProxyHandle>,
    ) -> Result<Provider, String> {
        let mut session = store::StoreSession::begin()?;
        let root = &mut session.root;
        let backup = root
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

        let data = root
            .apps
            .get_mut(app)
            .ok_or_else(|| format!("未知应用: {app}"))?;

        let (target_id, target) = {
            let (id, p) =
                Self::find_provider(data, query).ok_or_else(|| format!("未找到供应商: {query}"))?;
            (id.clone(), p.clone())
        };

        let target_is_official = store::is_official_provider_for_app(app, &target);
        validate_provider(app, &target)?;
        validate_wire_api(app, &target)?;
        let active_port = proxy_handle
            .map(proxy::ProxyHandle::current_port)
            .unwrap_or(proxy::DEFAULT_PORT);
        if !target_is_official
            && proxy::is_self_proxy_target(
                &target.extract_base_url(app).unwrap_or_default(),
                active_port,
            )
        {
            return Err("供应商 Base URL 指向当前 CLI 代理端口，已拒绝递归路由".into());
        }
        let current_id = data.current.clone();
        let current_is_official = current_id
            .as_ref()
            .and_then(|cur| data.providers.get(cur))
            .is_some_and(|provider| store::is_official_provider_for_app(app, provider));

        let proxy_on = proxy_handle.map(|h| h.is_routed(app)).unwrap_or(false);

        if proxy_on {
            let handle = proxy_handle.expect("running proxy must have a handle");
            if target_is_official {
                live::write_live(app, &target, backup)?;
                proxy::clear_target(&handle.targets, app);
            } else {
                let runtime_target = proxy::target_from_provider(app, &target)
                    .ok_or_else(|| format!("供应商 {} 缺少可转发的 Base URL", target.name))?;
                if proxy::is_self_proxy_target(&runtime_target.base_url, handle.current_port()) {
                    return Err("供应商 Base URL 指向当前 CLI 代理端口，已拒绝递归路由".into());
                }
                let live_is_proxy = live::proxy_port(app) == Some(handle.current_port());

                if current_is_official || current_id.is_none() || !live_is_proxy {
                    if let Some(current) = current_id.as_ref() {
                        if current != &target_id {
                            if let Some(old) = data.providers.get_mut(current) {
                                live::backfill(app, old);
                            }
                        }
                    }
                    proxy::set_target(&handle.targets, app, runtime_target);
                    let proxied = proxy::proxied_provider(app, &target, handle.current_port());
                    if let Err(e) = live::write_live(app, &proxied, backup) {
                        proxy::clear_target(&handle.targets, app);
                        return Err(e);
                    }
                } else {
                    proxy::set_target(&handle.targets, app, runtime_target);
                }
            }
            data.current = Some(target_id);
            sync_claude_plugin(plugin_on, app, target_is_official);
            sync_claude_desktop(
                desktop_on,
                app,
                target_is_official,
                Some(&target),
                proxy_handle,
            );
            self.commit_session(session)?;
            return Ok(target);
        }

        // 直连模式
        if let Some(cur) = data.current.clone() {
            if cur != target_id {
                if let Some(old) = data.providers.get_mut(&cur) {
                    live::backfill(app, old);
                }
            }
        }
        live::write_live(app, &target, backup)?;
        data.current = Some(target_id);
        sync_claude_plugin(plugin_on, app, target_is_official);
        sync_claude_desktop(
            desktop_on,
            app,
            target_is_official,
            Some(&target),
            proxy_handle,
        );
        self.commit_session(session)?;
        Ok(target)
    }

    /// 异步切换供应商（支持无缝热切与后台常驻代理守护进程）
    pub async fn switch_async(
        &self,
        app: &str,
        query: &str,
        proxy_mode: Option<bool>,
        port: Option<u16>,
    ) -> Result<(Provider, bool), String> {
        let port = port
            .or_else(|| daemon::preferred_port_for_app(app))
            .unwrap_or(proxy::DEFAULT_PORT);
        daemon::validate_port(port)?;
        // 共享 live 文件只能同时指向一个代理。默认/直连模式不能静默
        // 覆盖 GUI（默认 8899）或其它仍在监听的本地代理。
        if proxy_mode != Some(true) {
            if let Some(foreign_port) = daemon::active_foreign_proxy_port(app, port).await {
                return Err(active_foreign_proxy_error(app, port, foreign_port));
            }
        }
        let daemon_was_alive_before = daemon::is_running(port).await;
        // 代理进程可以同时服务多个应用；默认模式必须看当前 app 是否已路由，
        // 不能因为另一个 app 开着代理就意外把本 app 改成 localhost 配置。
        let app_was_routed_before = daemon_was_alive_before
            && daemon::get_status(port)
                .await
                .ok()
                .is_some_and(|status| status.routed_apps.iter().any(|routed| routed == app));
        let mut proxy_requested = proxy_mode.unwrap_or(app_was_routed_before);
        // 先在无锁快照上校验目标。不能持有 StoreSession 再启动 worker，
        // 因为 worker 启动时也需要读取 providers.json；但也不能在查询失败后
        // 先拉起一个无用的后台代理。
        let should_start_proxy = if proxy_requested {
            let preview_root = store::load_checked()?;
            let preview_data = preview_root
                .apps
                .get(app)
                .ok_or_else(|| format!("未知应用: {app}"))?;
            let (_, preview_provider) = Self::find_provider(preview_data, query)
                .ok_or_else(|| format!("未找到供应商: {query}"))?;
            validate_provider(app, preview_provider)?;
            validate_wire_api(app, preview_provider)?;
            if store::is_official_provider_for_app(app, preview_provider) {
                false
            } else {
                let target =
                    proxy::target_from_provider(app, preview_provider).ok_or_else(|| {
                        format!("供应商 {} 缺少可转发的 Base URL", preview_provider.name)
                    })?;
                if proxy::is_self_proxy_target(&target.base_url, port) {
                    return Err("供应商 Base URL 指向当前 CLI 代理端口，已拒绝递归路由".into());
                }
                true
            }
        } else {
            false
        };
        let mut started_here: Option<u32> = None;
        if should_start_proxy && !daemon_was_alive_before {
            // 启动工作进程必须发生在 StoreSession 之前；工作进程启动时也会读取
            // providers.json，不能让它等待当前进程持有的配置锁。
            let outcome = daemon::start_background_owned(port).await?;
            started_here = outcome.started.then_some(outcome.pid);
            // start_background_owned() 可能复用另一个并发终端刚启动的 worker。
            // 此时代理已经存在，后续失败回滚应按“已有代理”处理，不能停掉
            // 别的终端创建的 worker。
        }

        let mut session = match store::StoreSession::begin() {
            Ok(session) => session,
            Err(error) => {
                stop_started_proxy(port, started_here).await;
                return Err(error);
            }
        };

        // The initial status check intentionally happens before starting a
        // worker, but another CLI/GUI process may change the live route while
        // this command waits for the shared store lock. Refresh the ownership
        // and routing decision under that lock before touching live files.
        if proxy_mode != Some(true) {
            if let Some(foreign_port) = daemon::active_foreign_proxy_port(app, port).await {
                drop(session);
                stop_started_proxy(port, started_here).await;
                return Err(active_foreign_proxy_error(app, port, foreign_port));
            }
        }
        let latest_status = daemon::get_status(port).await.ok();
        let mut daemon_was_alive = latest_status.is_some();
        let mut app_was_routed = latest_status
            .as_ref()
            .is_some_and(|status| status.routed_apps.iter().any(|routed| routed == app));
        proxy_requested = proxy_mode.unwrap_or(app_was_routed);

        // `--proxy` is an explicit request. The worker may have been stopped
        // by another terminal while this command waited for providers.json's
        // cross-process lock. Starting a worker while holding StoreSession
        // would deadlock because the worker also reads providers.json, so
        // release the session, restore the daemon, and load a fresh snapshot.
        if proxy_mode == Some(true) && should_start_proxy && !daemon_was_alive {
            drop(session);
            // Any worker created by the first attempt is no longer reachable.
            // Only claim ownership of the worker created by this retry.
            let outcome = daemon::start_background_owned(port).await?;
            started_here = match started_here {
                Some(previous_pid) if previous_pid == outcome.pid => Some(previous_pid),
                _ if outcome.started => Some(outcome.pid),
                _ => None,
            };
            session = match store::StoreSession::begin() {
                Ok(session) => session,
                Err(error) => {
                    stop_started_proxy(port, started_here).await;
                    return Err(error);
                }
            };
            let restarted_status = daemon::get_status(port).await.ok();
            daemon_was_alive = restarted_status.is_some();
            app_was_routed = restarted_status
                .as_ref()
                .is_some_and(|status| status.routed_apps.iter().any(|routed| routed == app));
            proxy_requested = true;
            if !daemon_was_alive {
                drop(session);
                stop_started_proxy(port, started_here).await;
                return Err(format!("后台代理在端口 {port} 启动后未能保持运行"));
            }
        }
        let daemon_alive = daemon_was_alive;

        let root = &mut session.root;
        let backup = root
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

        let data = match root.apps.get_mut(app) {
            Some(data) => data,
            None => {
                drop(session);
                stop_started_proxy(port, started_here).await;
                return Err(format!("未知应用: {app}"));
            }
        };

        let (target_id, target) = {
            let (id, p) = match Self::find_provider(data, query) {
                Some(found) => found,
                None => {
                    drop(session);
                    stop_started_proxy(port, started_here).await;
                    return Err(format!("未找到供应商: {query}"));
                }
            };
            (id.clone(), p.clone())
        };

        if let Err(error) = validate_provider(app, &target) {
            drop(session);
            stop_started_proxy(port, started_here).await;
            return Err(error);
        }
        if let Err(error) = validate_wire_api(app, &target) {
            drop(session);
            stop_started_proxy(port, started_here).await;
            return Err(error);
        }
        let target_is_official = store::is_official_provider_for_app(app, &target);
        if !target_is_official
            && proxy::is_self_proxy_target(&target.extract_base_url(app).unwrap_or_default(), port)
        {
            drop(session);
            stop_started_proxy(port, started_here).await;
            return Err("供应商 Base URL 指向当前 CLI 代理端口，已拒绝递归路由".into());
        }
        let current_id = data.current.clone();
        let previous_provider = current_id
            .as_ref()
            .and_then(|id| data.providers.get(id))
            .cloned();
        let previous_target = if app_was_routed {
            previous_provider
                .as_ref()
                .filter(|provider| !store::is_official_provider_for_app(app, provider))
                .and_then(|provider| proxy::target_from_provider(app, provider))
        } else {
            None
        };
        let live_snapshot = match live::snapshot_app(app) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                drop(session);
                stop_started_proxy(port, started_here).await;
                return Err(error);
            }
        };

        let use_proxy = proxy_requested;

        if use_proxy && !target_is_official {
            // 代理可能刚刚被停止过：此时 current 仍然是第三方 provider，
            // 但 stop_proxy 已把 live 配置恢复成真实上游地址。是否需要重写
            // live 不能由 current 的 provider 类型推断，必须检查实际端口。
            let live_is_proxy = live::proxy_port(app) == Some(port);
            if let Some(cur) = current_id.as_ref() {
                if cur != &target_id {
                    if let Some(old) = data.providers.get_mut(cur) {
                        live::backfill(app, old);
                    }
                }
            }

            let runtime_target = match proxy::target_from_provider(app, &target) {
                Some(target) => target,
                None => {
                    drop(session);
                    stop_started_proxy(port, started_here).await;
                    return Err(format!("供应商 {} 缺少可转发的 Base URL", target.name));
                }
            };

            if let Err(error) = daemon::send_switch(port, app, Some(runtime_target)).await {
                drop(session);
                return Err(rollback_app_change(
                    error,
                    app,
                    port,
                    live_snapshot,
                    daemon_was_alive,
                    app_was_routed,
                    previous_target.clone(),
                    started_here,
                    previous_provider.clone(),
                )
                .await);
            }

            if !live_is_proxy {
                let proxied = proxy::proxied_provider(app, &target, port);
                if let Err(error) = live::write_live(app, &proxied, backup) {
                    drop(session);
                    return Err(rollback_app_change(
                        error,
                        app,
                        port,
                        live_snapshot,
                        daemon_was_alive,
                        app_was_routed,
                        previous_target.clone(),
                        started_here,
                        previous_provider.clone(),
                    )
                    .await);
                }
            }

            data.current = Some(target_id);
            if let Err(error) = self.commit_session(session) {
                return Err(rollback_app_change(
                    error,
                    app,
                    port,
                    live_snapshot,
                    daemon_was_alive,
                    app_was_routed,
                    previous_target,
                    started_here,
                    previous_provider.clone(),
                )
                .await);
            }
            sync_claude_plugin(plugin_on, app, target_is_official);
            if desktop_on && app == "claude" && claude_desktop::is_supported() {
                let result = claude_desktop::apply_proxy(&proxy::local_base(port, "claude"));
                if let Err(error) = result {
                    eprintln!("[z-switch] 同步 Claude Desktop 代理配置失败: {error}");
                }
            }
            return Ok((target, true));
        }

        // 直连模式。先写成功再解除路由，避免写盘失败时客户端已经失去
        // 原有的 localhost 路由目标。
        if let Some(cur) = current_id.as_ref() {
            if cur != &target_id {
                if let Some(old) = data.providers.get_mut(cur) {
                    live::backfill(app, old);
                }
            }
        }
        if let Err(error) = live::write_live(app, &target, backup) {
            drop(session);
            return Err(rollback_app_change(
                error,
                app,
                port,
                live_snapshot,
                daemon_was_alive,
                app_was_routed,
                previous_target.clone(),
                started_here,
                previous_provider.clone(),
            )
            .await);
        }
        if daemon_alive {
            if let Err(error) = daemon::send_switch(port, app, None).await {
                drop(session);
                return Err(rollback_app_change(
                    error,
                    app,
                    port,
                    live_snapshot,
                    daemon_was_alive,
                    app_was_routed,
                    previous_target.clone(),
                    started_here,
                    previous_provider.clone(),
                )
                .await);
            }
        }
        data.current = Some(target_id);
        if let Err(error) = self.commit_session(session) {
            return Err(rollback_app_change(
                error,
                app,
                port,
                live_snapshot,
                daemon_was_alive,
                app_was_routed,
                previous_target,
                started_here,
                previous_provider.clone(),
            )
            .await);
        }
        sync_claude_plugin(plugin_on, app, target_is_official);
        if desktop_on && app == "claude" && claude_desktop::is_supported() {
            let result = if target_is_official {
                claude_desktop::restore_official()
            } else {
                claude_desktop::apply_direct(&target)
            };
            if let Err(error) = result {
                eprintln!("[z-switch] 同步 Claude Desktop 直连配置失败: {error}");
            }
        }
        Ok((target, false))
    }

    /// 新增或保存供应商 (异步，支持热更代理)
    pub async fn save_provider_async(
        &self,
        app: &str,
        provider: Provider,
        port: Option<u16>,
    ) -> Result<Provider, String> {
        let port = port
            .or_else(|| daemon::preferred_port_for_app(app))
            .unwrap_or(proxy::DEFAULT_PORT);
        daemon::validate_port(port)?;

        let mut session = store::StoreSession::begin()?;
        let root = &mut session.root;
        // 路由状态必须在共享配置锁内重新读取。否则 stop/use/edit 并发时，
        // 可能依据旧状态写入直连配置，留下仍在代理中的客户端。
        let route_active = daemon::get_status(port)
            .await
            .ok()
            .is_some_and(|status| status.routed_apps.iter().any(|routed| routed == app));
        let backup = root
            .settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let desktop_on = root
            .settings
            .get("applyClaudeDesktop")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let id = provider.id.clone();
        validate_provider(app, &provider)?;
        validate_wire_api(app, &provider)?;
        if id.trim().is_empty() {
            return Err("供应商 ID 不能为空".into());
        }
        if store::official_provider_id(app) == Some(id.as_str()) {
            return Err("官方账号是系统卡片，不能覆盖修改".into());
        }
        let data = root.apps.entry(app.to_string()).or_default();
        if !data.order.contains(&id) {
            data.order.push(id.clone());
        }
        let is_current = data.current.as_deref() == Some(id.as_str());
        // 当前供应商的编辑会改写 live 配置。无论 CLI 自己是否还能读取到
        // route_active，都必须确认 live 文件没有被 GUI/其它代理接管；否则
        // 旧状态快照可能让本次写入覆盖外部代理的路由。
        if is_current {
            if let Some(foreign_port) = daemon::active_foreign_proxy_port(app, port).await {
                return Err(active_foreign_proxy_error(app, port, foreign_port));
            }
        }
        let previous_provider = if is_current {
            data.providers.get(&id).cloned()
        } else {
            None
        };
        let live_snapshot = if is_current {
            Some(live::snapshot_app(app)?)
        } else {
            None
        };
        let previous_target = if route_active {
            previous_provider
                .as_ref()
                .and_then(|old| proxy::target_from_provider(app, old))
        } else {
            None
        };
        if is_current
            && proxy::is_self_proxy_target(
                &provider.extract_base_url(app).unwrap_or_default(),
                port,
            )
        {
            return Err("供应商 Base URL 指向当前 CLI 代理端口，已拒绝递归路由".into());
        }
        data.providers.insert(id.clone(), provider.clone());
        if is_current {
            if route_active {
                let target = proxy::target_from_provider(app, &provider)
                    .ok_or_else(|| format!("供应商 {} 缺少可转发的 Base URL", provider.name))?;
                if let Err(error) = daemon::send_switch(port, app, Some(target)).await {
                    return Err(rollback_app_change(
                        error,
                        app,
                        port,
                        live_snapshot
                            .clone()
                            .expect("current provider has a live snapshot"),
                        true,
                        true,
                        previous_target.clone(),
                        None,
                        previous_provider.clone(),
                    )
                    .await);
                }
                let proxied = proxy::proxied_provider(app, &provider, port);
                if let Err(error) = live::write_live(app, &proxied, backup) {
                    return Err(rollback_app_change(
                        error,
                        app,
                        port,
                        live_snapshot
                            .clone()
                            .expect("current provider has a live snapshot"),
                        true,
                        true,
                        previous_target.clone(),
                        None,
                        previous_provider.clone(),
                    )
                    .await);
                }
            } else {
                if let Err(error) = live::write_live(app, &provider, backup) {
                    return Err(rollback_app_change(
                        error,
                        app,
                        port,
                        live_snapshot
                            .clone()
                            .expect("current provider has a live snapshot"),
                        false,
                        false,
                        None,
                        None,
                        previous_provider.clone(),
                    )
                    .await);
                }
            }
        }
        if let Err(error) = self.commit_session(session) {
            if let Some(snapshot) = live_snapshot {
                return Err(rollback_app_change(
                    error,
                    app,
                    port,
                    snapshot,
                    route_active,
                    route_active,
                    previous_target,
                    None,
                    previous_provider,
                )
                .await);
            }
            return Err(error);
        }
        if is_current && desktop_on && app == "claude" && claude_desktop::is_supported() {
            let result = if route_active {
                claude_desktop::apply_proxy(&proxy::local_base(port, "claude"))
            } else {
                claude_desktop::apply_direct(&provider)
            };
            if let Err(error) = result {
                eprintln!("[z-switch] 同步 Claude Desktop 配置失败: {error}");
            }
        }
        Ok(provider)
    }

    pub async fn add_provider_async(
        &self,
        app: &str,
        mut provider: Provider,
    ) -> Result<Provider, String> {
        validate_provider(app, &provider)?;
        validate_wire_api(app, &provider)?;
        let mut session = store::StoreSession::begin()?;
        let root = &mut session.root;
        let data = root.apps.entry(app.to_string()).or_default();
        let base_id = provider.id.clone();
        provider.id = unique_id(&data.providers, &base_id);
        let id = provider.id.clone();
        data.order.push(id.clone());
        data.providers.insert(id, provider.clone());
        self.commit_session(session)?;
        Ok(provider)
    }

    pub fn add_provider(&self, app: &str, mut provider: Provider) -> Result<Provider, String> {
        validate_provider(app, &provider)?;
        validate_wire_api(app, &provider)?;
        let mut session = store::StoreSession::begin()?;
        let root = &mut session.root;
        let data = root.apps.entry(app.to_string()).or_default();
        let base_id = provider.id.clone();
        provider.id = unique_id(&data.providers, &base_id);
        let id = provider.id.clone();
        data.order.push(id.clone());
        data.providers.insert(id, provider.clone());
        self.commit_session(session)?;
        Ok(provider)
    }

    /// 删除供应商
    pub async fn delete_provider(
        &self,
        app: &str,
        query: &str,
        active_mode: Option<&str>,
        port: Option<u16>,
    ) -> Result<String, String> {
        let port = port
            .or_else(|| daemon::preferred_port_for_app(app))
            .unwrap_or(proxy::DEFAULT_PORT);
        daemon::validate_port(port)?;
        let mut session = store::StoreSession::begin()?;
        let root = &mut session.root;
        // 与 save_provider_async 一样，不能使用拿配置锁之前的代理快照。
        let route_active = daemon::get_status(port)
            .await
            .ok()
            .is_some_and(|status| status.routed_apps.iter().any(|routed| routed == app));
        // 这里判断的是 live 文件是否仍是 localhost 占位，而不是当前端口
        // 是否还有 CLI PID。代理可能已经异常退出并清理了 PID 文件；删除
        // 当前供应商时仍应把残留 localhost 配置恢复成真实配置。
        let live_proxy_port = live::proxy_port(app);
        let backup = root
            .settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let desktop_on = root
            .settings
            .get("applyClaudeDesktop")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let plugin_on = root
            .settings
            .get("applyClaudePlugin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let data = root
            .apps
            .get_mut(app)
            .ok_or_else(|| format!("未知应用: {app}"))?;

        let (target_id, target_name, target_provider) = {
            let (id, p) =
                Self::find_provider(data, query).ok_or_else(|| format!("未找到供应商: {query}"))?;
            (id.clone(), p.name.clone(), p.clone())
        };

        if store::official_provider_id(app) == Some(target_id.as_str()) {
            return Err("官方账号是系统卡片，不能删除".into());
        }

        let is_current = data.current.as_deref() == Some(target_id.as_str());
        // 删除当前供应商同样可能恢复/改写 live 配置，不能只依赖 CLI
        // 当前端口的 route_active 快照判断归属。
        if is_current {
            if let Some(foreign_port) = daemon::active_foreign_proxy_port(app, port).await {
                return Err(active_foreign_proxy_error(app, port, foreign_port));
            }
        }
        let restore_original = is_current && active_mode == Some("restore");
        let live_snapshot = if is_current {
            Some(live::snapshot_app(app)?)
        } else {
            None
        };
        let previous_target = if route_active {
            proxy::target_from_provider(app, &target_provider)
        } else {
            None
        };
        if is_current {
            match active_mode {
                Some("keep") => {
                    if route_active || live_proxy_port.is_some() {
                        if let Err(error) = live::write_live(app, &target_provider, backup) {
                            return Err(rollback_app_change(
                                error,
                                app,
                                port,
                                live_snapshot
                                    .clone()
                                    .expect("current provider has a live snapshot"),
                                route_active,
                                route_active,
                                previous_target.clone(),
                                None,
                                Some(target_provider.clone()),
                            )
                            .await);
                        }
                    }
                }
                Some("restore") => {
                    if let Err(error) = original::restore_app(app) {
                        return Err(rollback_app_change(
                            error,
                            app,
                            port,
                            live_snapshot
                                .clone()
                                .expect("current provider has a live snapshot"),
                            route_active,
                            route_active,
                            previous_target.clone(),
                            None,
                            Some(target_provider.clone()),
                        )
                        .await);
                    }
                }
                _ => {
                    return Err(
                        "该供应商当前正在使用中，请指定处理方式 (--mode keep 或 --mode restore)"
                            .into(),
                    );
                }
            }
            if route_active {
                if let Err(error) = daemon::send_switch(port, app, None).await {
                    return Err(rollback_app_change(
                        error,
                        app,
                        port,
                        live_snapshot
                            .clone()
                            .expect("current provider has a live snapshot"),
                        true,
                        true,
                        previous_target.clone(),
                        None,
                        Some(target_provider.clone()),
                    )
                    .await);
                }
            }
            // 删除后 live 配置可能仍然保留（keep），但已不再由 z-switch
            // 管理，不能把 current 错标为官方账号。
            data.current = None;
        }

        data.providers.remove(&target_id);
        data.order.retain(|x| x != &target_id);
        if let Err(error) = self.commit_session(session) {
            if let Some(snapshot) = live_snapshot {
                return Err(rollback_app_change(
                    error,
                    app,
                    port,
                    snapshot,
                    route_active,
                    route_active,
                    previous_target,
                    None,
                    Some(target_provider),
                )
                .await);
            }
            return Err(error);
        }
        if restore_original {
            sync_claude_plugin(plugin_on, app, true);
            if desktop_on && app == "claude" && claude_desktop::is_supported() {
                if let Err(error) = claude_desktop::restore_official() {
                    eprintln!("[z-switch] 恢复 Claude Desktop 官方配置失败: {error}");
                }
            }
        } else if is_current && desktop_on && app == "claude" && claude_desktop::is_supported() {
            if let Err(error) = claude_desktop::apply_direct(&target_provider) {
                eprintln!("[z-switch] 恢复 Claude Desktop 直连配置失败: {error}");
            }
        }
        Ok(target_name)
    }

    /// 从当前环境导入
    pub fn import_live(&self) -> Result<Vec<String>, String> {
        let mut session = store::StoreSession::begin()?;
        let root = &mut session.root;
        let mut imported = Vec::new();
        if let Some(mut p) = live::import_claude() {
            let data = root.apps.entry("claude".into()).or_default();
            let id = unique_id(&data.providers, "imported-claude");
            p.id = id.clone();
            if !data.order.contains(&id) {
                data.order.push(id.clone());
            }
            data.providers.insert(id.clone(), p);
            data.current = Some(id);
            imported.push("Claude".into());
        }
        if let Some(mut p) = live::import_codex() {
            let data = root.apps.entry("codex".into()).or_default();
            let id = unique_id(&data.providers, "imported-codex");
            p.id = id.clone();
            if !data.order.contains(&id) {
                data.order.push(id.clone());
            }
            data.providers.insert(id.clone(), p);
            data.current = Some(id);
            imported.push("Codex".into());
        }
        if let Some(mut p) = live::import_grok() {
            let data = root.apps.entry("grok".into()).or_default();
            let id = unique_id(&data.providers, "imported-grok");
            p.id = id.clone();
            if !data.order.contains(&id) {
                data.order.push(id.clone());
            }
            data.providers.insert(id.clone(), p);
            data.current = Some(id);
            imported.push("Grok".into());
        }
        if imported.is_empty() {
            return Err("未在 ~/.claude、~/.codex 或 ~/.grok 找到可导入的有效中转配置".into());
        }
        self.commit_session(session)?;
        Ok(imported)
    }

    /// 一键恢复官方基线
    pub async fn restore_official_baseline(
        &self,
        app: &str,
        port: Option<u16>,
    ) -> Result<(), String> {
        if !proxy::PROXY_APPS.contains(&app) {
            return Err(format!("未知应用: {app}"));
        }
        let port = port
            .or_else(|| daemon::preferred_port_for_app(app))
            .unwrap_or(proxy::DEFAULT_PORT);
        daemon::validate_port(port)?;
        if let Some(foreign_port) = daemon::active_foreign_proxy_port(app, port).await {
            return Err(active_foreign_proxy_error(app, port, foreign_port));
        }
        let mut session = store::StoreSession::begin()?;
        let root = &mut session.root;
        let backup = root
            .settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let app_was_routed = daemon::get_status(port)
            .await
            .ok()
            .is_some_and(|status| status.routed_apps.iter().any(|routed| routed == app));
        let previous_provider = root
            .apps
            .get(app)
            .and_then(|data| data.current.as_ref().and_then(|id| data.providers.get(id)))
            .cloned();
        let previous_target = if app_was_routed {
            previous_provider
                .as_ref()
                .filter(|provider| !store::is_official_provider_for_app(app, provider))
                .and_then(|provider| proxy::target_from_provider(app, provider))
        } else {
            None
        };
        let live_snapshot = live::snapshot_app(app)?;
        if app == "grok" {
            original::restore_app(app)?;
        } else {
            live::write_official_baseline(app, backup)?;
        }
        if app_was_routed {
            if let Err(error) = daemon::send_switch(port, app, None).await {
                return Err(rollback_app_change(
                    error,
                    app,
                    port,
                    live_snapshot,
                    true,
                    true,
                    previous_target.clone(),
                    None,
                    previous_provider.clone(),
                )
                .await);
            }
        }
        let plugin_on = root
            .settings
            .get("applyClaudePlugin")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let desktop_on = root
            .settings
            .get("applyClaudeDesktop")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        // 恢复官方不仅要改 Claude/Codex/Grok 的 live 文件，还要撤销
        // Claude Code 插件放行标记和 Claude Desktop 的第三方网关配置。
        if let Some(data) = root.apps.get_mut(app) {
            data.current = store::official_provider_id(app).map(str::to_string);
        }
        if let Err(error) = self.commit_session(session) {
            return Err(rollback_app_change(
                error,
                app,
                port,
                live_snapshot,
                app_was_routed,
                app_was_routed,
                previous_target,
                None,
                previous_provider,
            )
            .await);
        }
        sync_claude_plugin(plugin_on, app, true);
        if desktop_on && app == "claude" && claude_desktop::is_supported() {
            if let Err(error) = claude_desktop::restore_official() {
                eprintln!("[z-switch] 恢复 Claude Desktop 官方配置失败: {error}");
            }
        }
        Ok(())
    }

    /// 环境诊断
    pub async fn diagnose(&self) -> Vec<DiagnosisResult> {
        let root = self.get_root();
        let mut list = Vec::new();
        for &app in &["claude", "codex", "grok"] {
            let snap = match app {
                "claude" => repair::read_claude(),
                "codex" => repair::read_codex(),
                "grok" => repair::read_grok(),
                _ => continue,
            };
            let placeholder = snap.key_is_placeholder;
            let live_proxy_port = live::proxy_port(app);
            let proxy_residue = live_proxy_port.is_some() || placeholder;
            // PID 文件可能在 worker 异常退出时已经丢失，但 live 配置仍保留
            // 实际监听端口。诊断必须优先使用这个端口，不能回退到默认端口
            // 后把另一个代理实例的路由状态误算给当前应用。
            let port = live_proxy_port
                .or_else(|| daemon::preferred_port_for_app(app))
                .unwrap_or(proxy::DEFAULT_PORT);
            let routed = daemon::get_status(port)
                .await
                .ok()
                .is_some_and(|status| status.routed_apps.iter().any(|routed| routed == app));
            let cli_proxy_alive = match live_proxy_port {
                Some(proxy_port) => daemon::is_running(proxy_port).await,
                None => false,
            };
            let external_proxy_alive = match live_proxy_port {
                Some(proxy_port) if !cli_proxy_alive => daemon::is_port_open(proxy_port).await,
                _ => false,
            };
            let current_name = root
                .apps
                .get(app)
                .and_then(|d| d.current.as_ref().and_then(|id| d.providers.get(id)))
                .map(|p| p.name.clone());

            let healthy = if proxy_residue {
                // A running route is only relevant when the live file actually
                // points at that route. A remote URL with a leftover placeholder
                // key must not be reported healthy just because another app
                // happens to be routed on the default port.
                live_proxy_port.is_some() && (routed || external_proxy_alive)
            } else {
                true
            };
            let issue = if !healthy {
                Some(if cli_proxy_alive && live_proxy_port.is_some() {
                    "CLI 代理正在运行，但当前应用没有有效路由目标".to_string()
                } else {
                    "检测到本地代理不可达或占位配置残留".to_string()
                })
            } else {
                None
            };
            list.push(DiagnosisResult {
                app: app.to_string(),
                current_name,
                base_url: snap.base_url,
                healthy,
                issue,
            });
        }
        list
    }

    /// 环境修复
    pub async fn repair_app(&self, app: &str, port: Option<u16>) -> Result<(), String> {
        if !proxy::PROXY_APPS.contains(&app) {
            return Err(format!("未知应用: {app}"));
        }
        let port = port
            .or_else(|| daemon::preferred_port_for_app(app))
            .unwrap_or(proxy::DEFAULT_PORT);
        daemon::validate_port(port)?;
        if let Some(foreign_port) = daemon::active_foreign_proxy_port(app, port).await {
            return Err(active_foreign_proxy_error(app, port, foreign_port));
        }
        let mut session = store::StoreSession::begin()?;
        let root = &mut session.root;
        let status = daemon::get_status(port).await.ok();
        if status
            .as_ref()
            .is_some_and(|status| status.routed_apps.iter().any(|routed| routed == app))
        {
            return Err(format!(
                "{app} 当前正在本地路由（属正常状态），如需直连请先执行 use --direct"
            ));
        }
        let live_snapshot = live::snapshot_app(app)?;
        let backup = root
            .settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let current = root.apps.get(app).and_then(|d| {
            d.current
                .as_ref()
                .and_then(|id| d.providers.get(id))
                .cloned()
        });
        match current {
            Some(provider) => {
                live::write_live(app, &provider, backup)?;
            }
            None => {
                original::restore_app(app)?;
            }
        }
        if let Err(error) = self.commit_session(session) {
            return Err(with_live_snapshot_rollback(
                error,
                &[(app.to_string(), live_snapshot)],
            ));
        }
        Ok(())
    }

    /// 停止 CLI 代理前先解除所有应用的本地路由，避免 live 配置指向
    /// 已经不存在的 localhost 端点。
    pub async fn stop_proxy(&self, port: u16) -> Result<(), String> {
        daemon::validate_port(port)?;
        let _lifecycle_lock = daemon::acquire_lifecycle_lock(port).await?;
        self.stop_proxy_locked(port).await
    }

    async fn stop_proxy_locked(&self, port: u16) -> Result<(), String> {
        daemon::validate_port(port)?;
        let mut runtime_routed_apps = Vec::new();
        let routed_apps = match daemon::get_status(port).await {
            Ok(status) => {
                runtime_routed_apps = status.routed_apps.clone();
                let mut apps = status.routed_apps;
                // worker 启动时可能因供应商配置不完整而没有建立内存 target，
                // 但 live 文件仍可能已经指向本地端口。停止代理也必须恢复这类
                // 残留，否则客户端会继续请求一个即将关闭的 localhost 端点。
                for app in proxy::PROXY_APPS {
                    if live::proxy_port(app) == Some(port)
                        && !apps.iter().any(|existing| existing == app)
                    {
                        apps.push((*app).to_string());
                    }
                }
                apps
            }
            Err(error) => {
                // 进程已经退出但 PID 文件或 live 配置仍在时，尽量把残留
                // 配置恢复为真实供应商；端口仍开放时则绝不触碰其它进程。
                if daemon::is_port_open(port).await {
                    return Err(format!("无法确认 CLI 代理状态，未停止代理: {error}"));
                }
                // 端口已关闭且 live 仍指向它时，这是可恢复的残留配置，
                // 不应因为 PID 文件已被异常退出的 worker 清掉而跳过恢复。
                proxy::PROXY_APPS
                    .iter()
                    .filter(|app| live::proxy_port(app) == Some(port))
                    .map(|app| (*app).to_string())
                    .collect()
            }
        };
        if routed_apps.is_empty() {
            return daemon::stop_locked(port).await;
        }

        // The in-memory route can outlive a manual/GUI edit of the live file.
        // Only restore files that still explicitly point at this CLI port;
        // otherwise stopping the daemon would overwrite an external direct
        // configuration or another proxy's localhost route.
        let managed_apps: Vec<String> = routed_apps
            .iter()
            .filter(|app| live::proxy_port(app) == Some(port))
            .cloned()
            .collect();
        for app in &routed_apps {
            if !managed_apps.iter().any(|managed| managed == app) {
                eprintln!(
                    "[z-switch] {app} 的 live 配置已不再指向 CLI 代理端口 {port}，停止时保留现有配置"
                );
            }
        }

        let session = store::StoreSession::begin()?;
        let root = &session.root;
        let backup = root
            .settings
            .get("backupBeforeWrite")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let plugin_on = root
            .settings
            .get("applyClaudePlugin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let desktop_on = root
            .settings
            .get("applyClaudeDesktop")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let claude_provider = if managed_apps.iter().any(|app| app == "claude") {
            root.apps.get("claude").and_then(|data| {
                data.current
                    .as_ref()
                    .and_then(|id| data.providers.get(id))
                    .cloned()
            })
        } else {
            None
        };

        let live_snapshots = managed_apps
            .iter()
            .map(|app| live::snapshot_app(app).map(|snapshot| (app.clone(), snapshot)))
            .collect::<Result<Vec<_>, String>>()?;
        for app in &managed_apps {
            let provider = root
                .apps
                .get(app)
                .and_then(|data| data.current.as_ref().and_then(|id| data.providers.get(id)))
                .ok_or_else(|| format!("{app} 没有可恢复的当前供应商，未停止代理"))?;
            if let Err(error) = live::write_live(app, provider, backup) {
                return Err(with_live_snapshot_rollback(error, &live_snapshots));
            }
        }

        for app in &routed_apps {
            if let Err(error) = daemon::send_switch(port, app, None).await {
                eprintln!("[z-switch] 清除 {app} 代理路由失败，继续停止进程: {error}");
            }
        }
        if desktop_on && managed_apps.iter().any(|app| app == "claude") {
            if let Some(provider) = claude_provider.as_ref() {
                let result = if store::is_official_provider_for_app("claude", provider) {
                    claude_desktop::restore_official()
                } else {
                    claude_desktop::apply_direct(provider)
                };
                if let Err(error) = result {
                    eprintln!("[z-switch] 恢复 Claude Desktop 直连配置失败: {error}");
                }
            }
        }
        if managed_apps.iter().any(|app| app == "claude") {
            let target_is_official = claude_provider
                .as_ref()
                .is_none_or(|provider| store::is_official_provider_for_app("claude", provider));
            sync_claude_plugin(plugin_on, "claude", target_is_official);
        }

        match daemon::stop_locked(port).await {
            Ok(()) => Ok(()),
            Err(error) if daemon::is_running(port).await => {
                // 停止信令或强杀失败时，代理仍在服务请求。前面已经把 live
                // 配置改回直连并清除了内存路由，必须恢复两者，否则现有
                // 客户端会继续请求旧 localhost，而代理已经没有目标。
                let mut rollback_errors = Vec::new();
                for (app, snapshot) in &live_snapshots {
                    if let Err(rollback_error) = live::restore_snapshot(snapshot) {
                        rollback_errors.push(format!("恢复 {app} live 配置失败: {rollback_error}"));
                    }
                }

                for app in &runtime_routed_apps {
                    let target = root
                        .apps
                        .get(app)
                        .and_then(|data| {
                            data.current.as_ref().and_then(|id| data.providers.get(id))
                        })
                        .and_then(|provider| proxy::target_from_provider(app, provider));
                    match target {
                        Some(target) => {
                            if let Err(rollback_error) =
                                daemon::send_switch(port, app, Some(target)).await
                            {
                                rollback_errors
                                    .push(format!("恢复 {app} 代理路由失败: {rollback_error}"));
                            }
                        }
                        None => rollback_errors.push(format!(
                            "恢复 {app} 代理路由失败: 当前供应商缺少有效 Base URL"
                        )),
                    }
                }

                if desktop_on && managed_apps.iter().any(|app| app == "claude") {
                    if let Err(rollback_error) =
                        claude_desktop::apply_proxy(&proxy::local_base(port, "claude"))
                    {
                        rollback_errors.push(format!(
                            "恢复 Claude Desktop 代理配置失败: {rollback_error}"
                        ));
                    }
                }
                if managed_apps.iter().any(|app| app == "claude") {
                    sync_claude_plugin(plugin_on, "claude", false);
                }

                if rollback_errors.is_empty() {
                    Err(format!("{error}; 已恢复仍在运行的代理路由和 live 配置"))
                } else {
                    Err(format!(
                        "{error}; 代理仍在运行，回滚失败: {}",
                        rollback_errors.join("；")
                    ))
                }
            }
            Err(error) => {
                // 进程已经消失时，live 直连配置是正确的最终状态；不要把
                // localhost 快照恢复回去，只报告停止过程的原始错误。
                Err(error)
            }
        }
    }

    pub async fn restart_proxy(&self, port: u16) -> Result<(), String> {
        daemon::validate_port(port)?;
        let _lifecycle_lock = daemon::acquire_lifecycle_lock(port).await?;
        let mut routed_apps = daemon::get_status(port)
            .await
            .ok()
            .map(|status| status.routed_apps)
            .unwrap_or_default();

        // 内存路由状态会在 worker 异常退出时丢失，但客户端 live 配置可能
        // 仍然指向这个端口。把这类应用一并纳入恢复，避免重启后代理虽然
        // 监听成功，客户端却继续命中一个没有 target 的本地路由。
        for app in proxy::PROXY_APPS {
            if live::proxy_port(app) == Some(port)
                && !routed_apps.iter().any(|routed| routed == app)
            {
                routed_apps.push((*app).to_string());
            }
        }

        if routed_apps.is_empty() {
            daemon::stop_locked(port).await?;
        } else {
            self.stop_proxy_locked(port).await?;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        daemon::start_background_locked(port).await?;

        let root = self.get_root();
        for app in routed_apps {
            let current_id = root
                .apps
                .get(&app)
                .and_then(|data| data.current.as_deref())
                .ok_or_else(|| format!("{app} 没有可恢复的当前供应商"))?
                .to_string();
            self.switch_async(&app, &current_id, Some(true), Some(port))
                .await
                .map_err(|error| format!("恢复 {app} 代理路由失败: {error}"))?;
        }
        Ok(())
    }
}

fn apply_bool_setting_side_effect(root: &Root, key: &str, value: bool) -> Result<(), String> {
    match key {
        "applyClaudePlugin" => {
            let managed = value
                && root
                    .apps
                    .get("claude")
                    .and_then(|data| data.current.as_ref())
                    .and_then(|id| {
                        root.apps
                            .get("claude")
                            .and_then(|data| data.providers.get(id))
                    })
                    .is_some_and(|provider| {
                        !store::is_official_provider_for_app("claude", provider)
                    });
            claude_ext::apply_primary_api_key(managed)
        }
        "skipClaudeOnboarding" => claude_ext::apply_onboarding_completed(value),
        "applyClaudeDesktop" => {
            if !claude_desktop::is_supported() {
                return Ok(());
            }
            let provider = root
                .apps
                .get("claude")
                .and_then(|data| data.current.as_ref().and_then(|id| data.providers.get(id)));
            if !value
                || provider
                    .is_none_or(|provider| store::is_official_provider_for_app("claude", provider))
            {
                claude_desktop::restore_official()
            } else if let Some(port) = live::proxy_port("claude") {
                claude_desktop::apply_proxy(&proxy::local_base(port, "claude"))
            } else {
                claude_desktop::apply_direct(provider.expect("provider checked above"))
            }
        }
        _ => Ok(()),
    }
}

async fn stop_started_proxy(port: u16, started_here: Option<u32>) {
    if let Some(pid) = started_here {
        if let Err(error) = daemon::stop_if_pid(port, pid).await {
            eprintln!("[z-switch] 清理本次启动的代理失败: {error}");
        }
    }
}

fn with_live_snapshot_rollback(
    original_error: String,
    snapshots: &[(String, live::AppLiveSnapshot)],
) -> String {
    let mut rollback_errors = Vec::new();
    for (app, snapshot) in snapshots {
        if let Err(error) = live::restore_snapshot(snapshot) {
            rollback_errors.push(format!("恢复 {app} live 配置失败: {error}"));
        }
    }
    if rollback_errors.is_empty() {
        format!("{original_error}; 已回滚本次已修改的 live 配置")
    } else {
        format!(
            "{original_error}; live 配置回滚失败: {}",
            rollback_errors.join("；")
        )
    }
}

#[allow(clippy::too_many_arguments)]
async fn rollback_app_change(
    original_error: String,
    app: &str,
    port: u16,
    snapshot: live::AppLiveSnapshot,
    daemon_was_alive: bool,
    app_was_routed: bool,
    previous_target: Option<proxy::AppTarget>,
    started_here: Option<u32>,
    fallback_provider: Option<Provider>,
) -> String {
    let mut rollback_errors = Vec::new();
    let desired_target = if app_was_routed {
        previous_target
    } else {
        None
    };

    if let Some(pid) = started_here {
        if let Err(error) = daemon::stop_if_pid(port, pid).await {
            rollback_errors.push(format!("停止本次启动的代理失败: {error}"));
        }
    }

    // stop_if_pid() 可能因为进程退出竞态返回错误，也可能端口被同一 CLI
    // 的新 worker 接管。只要代理仍能响应，就把原路由状态恢复到操作前。
    let can_restore_route = daemon_was_alive || started_here.is_some();
    let mut status = daemon::get_status(port).await.ok();
    if status.is_some() && can_restore_route {
        if let Err(error) = daemon::send_switch(port, app, desired_target).await {
            rollback_errors.push(format!("恢复原代理路由失败: {error}"));
        }
        status = daemon::get_status(port).await.ok();
    }

    let route_restored = app_was_routed
        && can_restore_route
        && status
            .as_ref()
            .is_some_and(|current| current.routed_apps.iter().any(|routed| routed == app));
    let snapshot_port = live::snapshot_proxy_port(&snapshot, app);

    // 只有代理仍然存活且原 app 路由确实恢复时，localhost 快照才是可用
    // 的。否则优先写回真实 provider，避免恢复一个已经无人监听的地址。
    let restore_snapshot = snapshot_port != Some(port) || route_restored;
    if restore_snapshot {
        if let Err(error) = live::restore_snapshot(&snapshot) {
            rollback_errors.push(format!("恢复原 live 配置失败: {error}"));
        }
    } else if let Some(provider) = fallback_provider {
        if let Err(error) = live::write_live(app, &provider, false) {
            rollback_errors.push(format!("代理已不可用，恢复真实 live 配置失败: {error}"));
        }
    } else {
        rollback_errors.push(format!(
            "代理已不可用，且没有可用于恢复 {app} 的真实供应商配置"
        ));
    }

    if rollback_errors.is_empty() {
        original_error
    } else {
        format!("{original_error}; 回滚失败: {}", rollback_errors.join("；"))
    }
}

pub struct DiagnosisResult {
    pub app: String,
    pub current_name: Option<String>,
    pub base_url: Option<String>,
    pub healthy: bool,
    pub issue: Option<String>,
}

fn sync_claude_plugin(plugin_on: bool, app: &str, target_is_official: bool) {
    if plugin_on && app == "claude" {
        let _ = claude_ext::apply_primary_api_key(!target_is_official);
    }
}

#[allow(dead_code)]
fn sync_claude_desktop(
    desktop_on: bool,
    app: &str,
    target_is_official: bool,
    provider: Option<&Provider>,
    proxy_handle: Option<&proxy::ProxyHandle>,
) {
    if !desktop_on || app != "claude" || !claude_desktop::is_supported() {
        return;
    }
    let _ = if target_is_official {
        claude_desktop::restore_official()
    } else if proxy_handle.map(|h| h.is_routed("claude")).unwrap_or(false) {
        let handle = proxy_handle.unwrap();
        claude_desktop::apply_proxy(&proxy::local_base(handle.current_port(), "claude"))
    } else if let Some(p) = provider {
        claude_desktop::apply_direct(p)
    } else {
        return;
    };
}

fn import_live_in_place(root: &mut Root) -> bool {
    let mut touched = false;
    if let Some(mut p) = live::import_claude() {
        let data = root.apps.entry("claude".into()).or_default();
        let id = unique_id(&data.providers, "imported-current");
        p.id = id.clone();
        if !data.order.contains(&id) {
            data.order.push(id.clone());
        }
        data.providers.insert(id.clone(), p);
        data.current = Some(id);
        touched = true;
    }
    if let Some(mut p) = live::import_codex() {
        let data = root.apps.entry("codex".into()).or_default();
        let id = unique_id(&data.providers, "imported-current");
        p.id = id.clone();
        if !data.order.contains(&id) {
            data.order.push(id.clone());
        }
        data.providers.insert(id.clone(), p);
        data.current = Some(id);
        touched = true;
    }
    if let Some(mut p) = live::import_grok() {
        let data = root.apps.entry("grok".into()).or_default();
        let id = unique_id(&data.providers, "imported-current");
        p.id = id.clone();
        if !data.order.contains(&id) {
            data.order.push(id.clone());
        }
        data.providers.insert(id.clone(), p);
        data.current = Some(id);
        touched = true;
    }
    touched
}

pub fn provider_id_from_name(name: &str) -> String {
    let id = name
        .trim()
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric(), "-")
        .trim_matches('-')
        .to_string();
    if id.is_empty() {
        "provider".to_string()
    } else {
        id
    }
}

pub fn unique_id(providers: &std::collections::HashMap<String, Provider>, base: &str) -> String {
    let base = if base.trim().is_empty() {
        "provider"
    } else {
        base
    };
    if !providers.contains_key(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}-{n}");
        if !providers.contains_key(&cand) {
            return cand;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn provider(id: &str, name: &str) -> Provider {
        Provider {
            id: id.into(),
            name: name.into(),
            category: Some("custom".into()),
            settings_config: json!({ "env": {} }),
            meta: json!({}),
            failover: json!({}),
        }
    }

    #[test]
    fn find_provider_uses_map_key_and_stable_order() {
        let data = store::AppData {
            current: None,
            order: vec!["second".into(), "first".into()],
            providers: HashMap::from([
                ("first".into(), provider("stale-first", "Relay One")),
                ("second".into(), provider("stale-second", "Relay Two")),
            ]),
        };

        let (exact_id, _) = SwitchService::find_provider(&data, "first").unwrap();
        assert_eq!(exact_id, "first");

        let (fuzzy_id, _) = SwitchService::find_provider(&data, "relay").unwrap();
        assert_eq!(fuzzy_id, "second");
        assert!(SwitchService::find_provider(&data, "   ").is_none());
    }

    #[test]
    fn bool_setting_keys_are_explicitly_whitelisted() {
        assert!(BOOL_SETTING_KEYS.contains(&"backupBeforeWrite"));
        assert!(BOOL_SETTING_KEYS.contains(&"applyClaudePlugin"));
        assert!(BOOL_SETTING_KEYS.contains(&"skipClaudeOnboarding"));
        assert!(BOOL_SETTING_KEYS.contains(&"applyClaudeDesktop"));
        assert!(!BOOL_SETTING_KEYS.contains(&"unexpectedSetting"));
    }

    #[test]
    fn provider_validation_rejects_proxy_placeholder_credentials() {
        let claude = Provider {
            id: "relay".into(),
            name: "Relay".into(),
            category: Some("custom".into()),
            settings_config: json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://relay.example",
                    "ANTHROPIC_AUTH_TOKEN": proxy::PLACEHOLDER_KEY
                }
            }),
            meta: json!({}),
            failover: json!({}),
        };
        assert!(validate_provider("claude", &claude).is_err());

        let codex = Provider {
            id: "relay".into(),
            name: "Relay".into(),
            category: Some("custom".into()),
            settings_config: json!({
                "auth": { "OPENAI_API_KEY": proxy::PLACEHOLDER_KEY },
                "config": "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://relay.example\"\n"
            }),
            meta: json!({}),
            failover: json!({}),
        };
        assert!(validate_provider("codex", &codex).is_err());

        let grok = Provider {
            id: "relay".into(),
            name: "Relay".into(),
            category: Some("custom".into()),
            settings_config: json!({
                "config": "[models]\ndefault = \"active\"\n[endpoints]\nmodels_base_url = \"https://relay.example\"\n[model.\"active\"]\napi_key = \"z-switch-proxy\"\n"
            }),
            meta: json!({}),
            failover: json!({}),
        };
        assert!(validate_provider("grok", &grok).is_err());

        let grok_auth = Provider {
            settings_config: json!({
                "auth": { "GROK_API_KEY": proxy::PLACEHOLDER_KEY },
                "config": "[endpoints]\nmodels_base_url = \"https://relay.example\"\n"
            }),
            ..grok.clone()
        };
        assert!(validate_provider("grok", &grok_auth).is_err());

        let xai_auth = Provider {
            settings_config: json!({
                "auth": { "XAI_API_KEY": proxy::PLACEHOLDER_KEY },
                "config": "[endpoints]\nmodels_base_url = \"https://relay.example\"\n"
            }),
            ..grok
        };
        assert!(validate_provider("grok", &xai_auth).is_err());
    }
}
