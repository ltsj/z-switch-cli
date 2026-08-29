//! 核心业务服务层：串联配置读写、切换、测速、生态同步与自愈。
use std::sync::Mutex;
use serde_json::Value;

use crate::claude_desktop;
use crate::claude_ext;
use crate::config;
use crate::connectivity;
use crate::live;
use crate::official;
use crate::original;
use crate::proxy;
use crate::repair;
use crate::store::{self, Provider, Root};

pub struct SwitchService {
    pub root: Mutex<Root>,
}

impl SwitchService {
    pub fn new() -> Self {
        let mut root = store::load();
        let mut root_changed = root.ensure_official_providers();

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

        let initial_import_done = root
            .settings
            .get("initialImportDone")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if snapshot_ready && !initial_import_done {
            if !root.has_non_official_provider() {
                let touched = import_live_in_place(&mut root);
                if touched {
                    root_changed = true;
                }
            }
            if let Some(settings) = root.settings.as_object_mut() {
                settings.insert("initialImportDone".into(), Value::Bool(true));
            }
            root_changed = true;
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
            let _ = store::save(&root);
        }

        Self {
            root: Mutex::new(root),
        }
    }

    pub fn get_root(&self) -> Root {
        self.root.lock().unwrap().clone()
    }

    pub fn backup_flag(&self) -> bool {
        let root = self.root.lock().unwrap();
        root.settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    pub fn find_provider<'a>(
        data: &'a store::AppData,
        query: &str,
    ) -> Option<(&'a String, &'a Provider)> {
        if let Some(p) = data.providers.get(query) {
            return Some((&p.id, p));
        }
        let q_lower = query.to_lowercase();
        // 精确匹配名称
        if let Some(entry) = data
            .providers
            .iter()
            .find(|(_, p)| p.name.to_lowercase() == q_lower)
        {
            return Some(entry);
        }
        // 包含匹配名称
        if let Some(entry) = data
            .providers
            .iter()
            .find(|(_, p)| p.name.to_lowercase().contains(&q_lower) || p.id.to_lowercase().contains(&q_lower))
        {
            return Some(entry);
        }
        None
    }

    /// 切换供应商
    pub fn switch(
        &self,
        app: &str,
        query: &str,
        proxy_handle: Option<&proxy::ProxyHandle>,
    ) -> Result<Provider, String> {
        let mut root = self.root.lock().unwrap();
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
            let (id, p) = Self::find_provider(data, query)
                .ok_or_else(|| format!("未找到供应商: {query}"))?;
            (id.clone(), p.clone())
        };

        let target_is_official = store::is_official_provider(&target);
        let current_id = data.current.clone();
        let current_is_official = current_id
            .as_ref()
            .and_then(|cur| data.providers.get(cur))
            .is_some_and(store::is_official_provider);

        let proxy_on = proxy_handle.map(|h| h.is_routed(app)).unwrap_or(false);

        if proxy_on {
            let handle = proxy_handle.expect("running proxy must have a handle");
            if target_is_official {
                live::write_live(app, &target, backup)?;
                proxy::clear_target(&handle.targets, app);
            } else {
                let runtime_target = proxy::target_from_provider(app, &target)
                    .ok_or_else(|| format!("供应商 {} 缺少可转发的 Base URL", target.name))?;

                if current_is_official || current_id.is_none() {
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
            sync_claude_desktop(desktop_on, app, target_is_official, Some(&target), proxy_handle);
            store::save(&root)?;
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
        sync_claude_desktop(desktop_on, app, target_is_official, Some(&target), proxy_handle);
        store::save(&root)?;
        Ok(target)
    }

    /// 新增或保存供应商
    pub fn save_provider(&self, app: &str, provider: Provider) -> Result<Provider, String> {
        let mut root = self.root.lock().unwrap();
        let backup = root
            .settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let id = provider.id.clone();
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
        data.providers.insert(id.clone(), provider.clone());
        if is_current {
            live::write_live(app, &provider, backup)?;
        }
        store::save(&root)?;
        Ok(provider)
    }

    /// 删除供应商
    pub fn delete_provider(
        &self,
        app: &str,
        query: &str,
        active_mode: Option<&str>,
    ) -> Result<String, String> {
        let mut root = self.root.lock().unwrap();
        let data = root
            .apps
            .get_mut(app)
            .ok_or_else(|| format!("未知应用: {app}"))?;

        let (target_id, target_name) = {
            let (id, p) = Self::find_provider(data, query)
                .ok_or_else(|| format!("未找到供应商: {query}"))?;
            (id.clone(), p.name.clone())
        };

        if store::official_provider_id(app) == Some(target_id.as_str()) {
            return Err("官方账号是系统卡片，不能删除".into());
        }

        let is_current = data.current.as_deref() == Some(target_id.as_str());
        if is_current {
            match active_mode {
                Some("keep") => {}
                Some("restore") => {
                    original::restore_app(app)?;
                }
                _ => {
                    return Err("该供应商当前正在使用中，请指定处理方式 (--mode keep 或 --mode restore)".into());
                }
            }
            data.current = None;
        }

        data.providers.remove(&target_id);
        data.order.retain(|x| x != &target_id);
        store::save(&root)?;
        Ok(target_name)
    }

    /// 从当前环境导入
    pub fn import_live(&self) -> Result<Vec<String>, String> {
        let mut root = self.root.lock().unwrap();
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
        store::save(&root)?;
        Ok(imported)
    }

    /// 一键恢复官方基线
    pub fn restore_official_baseline(&self, app: &str) -> Result<(), String> {
        let mut root = self.root.lock().unwrap();
        let backup = root
            .settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        live::write_official_baseline(app, backup)?;
        let official_id = store::official_provider_id(app)
            .ok_or_else(|| format!("应用 {app} 无官方账号卡片"))?;
        if let Some(data) = root.apps.get_mut(app) {
            data.current = Some(official_id.to_string());
        }
        store::save(&root)?;
        Ok(())
    }

    /// 环境诊断
    pub fn diagnose(&self) -> Vec<DiagnosisResult> {
        let root = self.root.lock().unwrap();
        let mut list = Vec::new();
        for &app in &["claude", "codex"] {
            let snap = if app == "claude" {
                repair::read_claude()
            } else {
                repair::read_codex()
            };
            let localhost = snap
                .base_url
                .as_deref()
                .map(repair::is_localhost)
                .unwrap_or(false);
            let placeholder = snap.key_is_placeholder;
            let current_name = root
                .apps
                .get(app)
                .and_then(|d| d.current.as_ref().and_then(|id| d.providers.get(id)))
                .map(|p| p.name.clone());

            let healthy = !localhost && !placeholder;
            let issue = if !healthy {
                Some("检测到本地代理占位残留 (127.0.0.1 或占位 Key)".to_string())
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
    pub fn repair_app(&self, app: &str) -> Result<(), String> {
        let root = self.root.lock().unwrap();
        let backup = root
            .settings
            .get("backupBeforeWrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let current = root
            .apps
            .get(app)
            .and_then(|d| d.current.as_ref().and_then(|id| d.providers.get(id)).cloned());
        match current {
            Some(provider) => live::write_live(app, &provider, backup)?,
            None => original::restore_app(app)?,
        }
        store::save(&root)?;
        Ok(())
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

pub fn unique_id(providers: &std::collections::HashMap<String, Provider>, base: &str) -> String {
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
