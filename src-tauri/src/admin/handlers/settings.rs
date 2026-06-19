//! `/api/settings` + `/api/config/*` —— 应用配置文件 / 备份 / 导入导出.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use axum::{http::StatusCode, response::IntoResponse, Json};
use codex_app_transfer_registry::{
    config_dir, normalize_model_mappings, RawConfig, DEFAULT_UPDATE_URL,
};
use serde_json::{json, Value};

#[cfg(test)]
use super::super::registry_io::save_for_test as save_registry;
use super::super::registry_io::{load as load_registry, with_config_write, ConfigMutation};
use super::common::{err, random_hex, APP_VERSION};

pub(super) fn ensure_settings_object(cfg: &mut RawConfig) -> &mut serde_json::Map<String, Value> {
    let obj = cfg.as_object_mut().expect("registry root is object");
    obj.entry("settings".to_owned())
        .or_insert_with(|| json!({}));
    obj.get_mut("settings")
        .and_then(|v| v.as_object_mut())
        .expect("settings is object")
}

pub(super) fn app_config_dir() -> Result<PathBuf, String> {
    config_dir().ok_or_else(|| "cannot locate user config directory".to_owned())
}

pub(super) fn app_config_file() -> Result<PathBuf, String> {
    Ok(app_config_dir()?.join("config.json"))
}

pub(super) fn app_backup_dir() -> Result<PathBuf, String> {
    Ok(app_config_dir()?.join("backups"))
}

pub(super) fn system_time_iso_seconds(time: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = time.into();
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

pub(super) fn default_config_value() -> Value {
    json!({
        "version": APP_VERSION,
        "activeProvider": null,
        "gatewayApiKey": null,
        "providers": [],
        "settings": {
            "theme": "default",
            "language": "zh",
            "proxyPort": 18080,
            "adminPort": 18081,
           "autoStart": false,
           "autoApplyOnStart": true,
           "exposeAllProviderModels": false,
           "showGrayProviders": false,
           "restoreCodexOnExit": true,
           "mcpCredentialsPortableStore": true,
            // [MOC-257 review] **不**在默认配置里 seed legacy `autoUnlockCodexPlugins`(已废弃,三态取代)——
            // 否则 normalize_imported_config 给缺该键的导入配置补 false,migrate 会把这个**生成的默认**当成用户
            // 显式 opt-out 迁成 off,绕过「无真账号→synthetic」缺键默认。缺键 = migrate None = 走默认推导。
            "autoWakeCodexPet": true,
           "updateUrl": DEFAULT_UPDATE_URL
        }
    })
}

pub(super) fn normalize_imported_provider(provider: &Value) -> Option<Value> {
    let src = provider.as_object()?;
    let mut normalized = src.clone();
    let provider_id = normalized.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let safe_id: String = provider_id
        .chars()
        .filter(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
        .take(64)
        .collect();
    normalized.insert(
        "id".into(),
        Value::String(if safe_id.is_empty() {
            random_hex(4)
        } else {
            safe_id
        }),
    );
    normalized
        .entry("name")
        .or_insert_with(|| Value::String("Unnamed Provider".into()));
    normalized
        .entry("baseUrl")
        .or_insert_with(|| Value::String(String::new()));
    normalized
        .entry("authScheme")
        .or_insert_with(|| Value::String("bearer".into()));
    // import_config 兜底:缺 apiFormat 字段(v1.x 备份 / 第三方手编 JSON)
    // 一律落 "openai_chat",跟 schema serde default / add_provider 对齐。
    normalized
        .entry("apiFormat")
        .or_insert_with(|| Value::String("openai_chat".into()));
    normalized
        .entry("apiKey")
        .or_insert_with(|| Value::String(String::new()));
    normalized
        .entry("extraHeaders")
        .or_insert_with(|| json!({}));
    normalized
        .entry("modelCapabilities")
        .or_insert_with(|| json!({}));
    normalized
        .entry("requestOptions")
        .or_insert_with(|| json!({}));
    normalized.entry("isBuiltin").or_insert(Value::Bool(false));
    normalized
        .entry("sortIndex")
        .or_insert(Value::Number(0.into()));

    let models = serde_json::to_value(normalize_model_mappings(normalized.get("models"))).ok()?;
    normalized.insert("models".into(), models);
    Some(Value::Object(normalized))
}

pub(super) fn normalize_imported_config(data: &Value) -> Result<Value, String> {
    let root = data
        .as_object()
        .ok_or_else(|| "config file must be a JSON object".to_owned())?;
    let source = root
        .get("config")
        .and_then(|v| v.as_object())
        .map(|obj| Value::Object(obj.clone()))
        .unwrap_or_else(|| data.clone());
    let source_obj = source
        .as_object()
        .ok_or_else(|| "config file must be a JSON object".to_owned())?;

    let mut normalized = default_config_value();
    {
        let obj = normalized.as_object_mut().expect("default config object");
        for key in [
            "version",
            "activeProvider",
            "gatewayApiKey",
            "providers",
            "settings",
        ] {
            if let Some(value) = source_obj.get(key) {
                obj.insert(key.to_owned(), value.clone());
            }
        }
        obj.insert(
            "version".into(),
            source_obj
                .get("version")
                .cloned()
                .unwrap_or_else(|| Value::String(APP_VERSION.to_owned())),
        );
    }

    let mut settings = default_config_value()
        .get("settings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let (Some(settings_obj), Some(imported)) = (
        settings.as_object_mut(),
        source_obj.get("settings").and_then(|v| v.as_object()),
    ) {
        for (key, value) in imported {
            settings_obj.insert(key.clone(), value.clone());
        }
    }
    normalized["settings"] = settings;

    let providers = source_obj
        .get("providers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "providers 必须是数组".to_owned())?;
    let mut normalized_providers = Vec::new();
    let mut seen_ids = HashSet::new();
    for provider in providers {
        let Some(mut normalized_provider) = normalize_imported_provider(provider) else {
            continue;
        };
        let provider_id = normalized_provider
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if !seen_ids.insert(provider_id.clone()) {
            if let Some(obj) = normalized_provider.as_object_mut() {
                obj.insert(
                    "id".into(),
                    Value::String(format!("{provider_id}-{}", random_hex(2))),
                );
            }
        }
        if let Some(id) = normalized_provider.get("id").and_then(|v| v.as_str()) {
            seen_ids.insert(id.to_owned());
        }
        normalized_providers.push(normalized_provider);
    }
    normalized["providers"] = Value::Array(normalized_providers);

    let provider_ids: HashSet<String> = normalized["providers"]
        .as_array()
        .map(|providers| {
            providers
                .iter()
                .filter_map(|p| p.get("id").and_then(|v| v.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let active_provider = source_obj.get("activeProvider").and_then(|v| v.as_str());
    normalized["activeProvider"] = if let Some(active) = active_provider {
        if provider_ids.contains(active) {
            Value::String(active.to_owned())
        } else {
            normalized["providers"]
                .as_array()
                .and_then(|providers| providers.first())
                .and_then(|p| p.get("id"))
                .cloned()
                .unwrap_or(Value::Null)
        }
    } else {
        normalized["providers"]
            .as_array()
            .and_then(|providers| providers.first())
            .and_then(|p| p.get("id"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    if let Some(key) = source_obj.get("gatewayApiKey").filter(|v| !v.is_null()) {
        normalized["gatewayApiKey"] = key.clone();
    }

    Ok(normalized)
}

pub(super) fn preserve_existing_provider_secrets(imported: &mut Value, current: &Value) {
    let Some(imported_providers) = imported.get_mut("providers").and_then(|v| v.as_array_mut())
    else {
        return;
    };
    let current_providers = current
        .get("providers")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    for provider in imported_providers {
        let Some(provider_obj) = provider.as_object_mut() else {
            continue;
        };
        let Some(provider_id) = provider_obj.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(current_provider) = current_providers
            .iter()
            .find(|item| item.get("id").and_then(|v| v.as_str()) == Some(provider_id))
            .and_then(|v| v.as_object())
        else {
            continue;
        };

        let imported_key_is_blank = provider_obj
            .get("apiKey")
            .and_then(|v| v.as_str())
            .map(|s| s.is_empty())
            .unwrap_or(true);
        if imported_key_is_blank {
            if let Some(existing_key) = current_provider
                .get("apiKey")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                provider_obj.insert("apiKey".into(), Value::String(existing_key.to_owned()));
            }
        }

        let imported_headers_empty = provider_obj
            .get("extraHeaders")
            .and_then(|v| v.as_object())
            .map(|obj| obj.is_empty())
            .unwrap_or(true);
        if imported_headers_empty {
            if let Some(existing_headers) = current_provider
                .get("extraHeaders")
                .and_then(|v| v.as_object())
                .filter(|obj| !obj.is_empty())
            {
                provider_obj.insert(
                    "extraHeaders".into(),
                    Value::Object(existing_headers.clone()),
                );
            }
        }
    }
}

pub(super) fn create_config_backup(reason: &str) -> Result<Value, String> {
    let backup_dir = app_backup_dir()?;
    fs::create_dir_all(&backup_dir).map_err(|e| format!("create backup directory failed: {e}"))?;
    let config_file = app_config_file()?;
    if !config_file.exists() {
        // ensure-config-exists:走 with_config_write 让 load(synthesize default)
        // + save 在 lock 内 atomic,防与并发 RMW 重复创建文件 race(尽管 load
        // 默认 JSON 同样,race window 写竞争仍可能让 fs::copy 后续 read 半文件)
        with_config_write(|_cfg| Ok(ConfigMutation::Modified(())))?;
    }

    let safe_reason: String = reason
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
        .take(32)
        .collect();
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%f");
    let filename = format!(
        "config-{timestamp}-{}-{}.json",
        if safe_reason.is_empty() {
            "manual"
        } else {
            safe_reason.as_str()
        },
        random_hex(2)
    );
    let target = backup_dir.join(&filename);
    fs::copy(&config_file, &target).map_err(|e| format!("copy config backup failed: {e}"))?;
    let stat = fs::metadata(&target).map_err(|e| format!("read backup metadata failed: {e}"))?;
    Ok(json!({
        "name": filename,
        "size": stat.len(),
        "createdAt": system_time_iso_seconds(stat.modified().unwrap_or_else(|_| SystemTime::now())),
    }))
}

pub(super) fn list_config_backups() -> Result<Vec<Value>, String> {
    let backup_dir = app_backup_dir()?;
    fs::create_dir_all(&backup_dir).map_err(|e| format!("create backup directory failed: {e}"))?;
    let mut backups = Vec::new();
    let entries =
        fs::read_dir(&backup_dir).map_err(|e| format!("read backup directory failed: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") || !path.is_file() {
            continue;
        }
        let stat = match fs::metadata(&path) {
            Ok(stat) => stat,
            Err(_) => continue,
        };
        let name = match path.file_name().and_then(|v| v.to_str()) {
            Some(name) => name.to_owned(),
            None => continue,
        };
        backups.push(json!({
            "name": name,
            "size": stat.len(),
            "createdAt": system_time_iso_seconds(stat.modified().unwrap_or_else(|_| SystemTime::now())),
        }));
    }
    backups.sort_by(|a, b| {
        let a = a.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
        let b = b.get("createdAt").and_then(|v| v.as_str()).unwrap_or("");
        b.cmp(a)
    });
    Ok(backups)
}

// ── /api/settings ────────────────────────────────────────────────────

pub async fn get_settings() -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let settings = cfg.get("settings").cloned().unwrap_or_else(|| json!({}));
    Json(settings).into_response()
}

pub async fn save_settings(Json(input): Json<Value>) -> impl IntoResponse {
    let result = with_config_write(|cfg| {
        // #MOC-62:记下旧值,只在 mcpCredentialsPortableStore 真变了才触发即时生效
        // (避免改主题等无关 settings 也去写 config.toml)。
        let old_portable = cfg
            .get("settings")
            .and_then(|s| s.get("mcpCredentialsPortableStore"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        // [MOC-100 P2-3] 同理记下 autoUnlockCodexPlugins 旧值,真变了才在写后
        // 同步 start/stop daemon(默认 true,跟 main.rs 启动 gating 对齐)。
        let old_auto_unlock = cfg
            .get("settings")
            .and_then(|s| s.get("autoUnlockCodexPlugins"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        // MOC-144:记下 webFetchBackend 旧值, 真变了才在写后注册/移除 mcp_server。
        let old_web_fetch = cfg
            .get("settings")
            .and_then(|s| s.get("webFetchBackend"))
            .and_then(Value::as_str)
            .unwrap_or(codex_app_transfer_registry::schema::DEFAULT_WEB_FETCH_BACKEND)
            .to_string();
        let s = ensure_settings_object(cfg);
        if let Some(obj) = input.as_object() {
            for (k, v) in obj {
                s.insert(k.clone(), v.clone());
            }
        }
        let settings = cfg.get("settings").cloned().unwrap_or_else(|| json!({}));
        let new_portable = settings
            .get("mcpCredentialsPortableStore")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let portable_changed = (new_portable != old_portable).then_some(new_portable);
        let new_auto_unlock = settings
            .get("autoUnlockCodexPlugins")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let auto_unlock_changed = (new_auto_unlock != old_auto_unlock).then_some(new_auto_unlock);
        let new_web_fetch = settings
            .get("webFetchBackend")
            .and_then(Value::as_str)
            .unwrap_or(codex_app_transfer_registry::schema::DEFAULT_WEB_FETCH_BACKEND)
            .to_string();
        let web_fetch_changed = (new_web_fetch != old_web_fetch).then_some(new_web_fetch);
        Ok(ConfigMutation::Modified((
            settings,
            portable_changed,
            auto_unlock_changed,
            web_fetch_changed,
        )))
    });
    match result {
        Ok((settings, portable_changed, auto_unlock_changed, web_fetch_changed)) => {
            // #262:settings.language 改动后 hot reload 到 adapters 全局,
            // 让接下来的 prompt 注入跟新语言一致(用户切语言无需重启 transfer)。
            sync_user_language_from_settings(&settings);
            // hideDockIcon 改动当场生效(macOS 切 activation policy 显/隐 Dock 图标,无需重启)。
            crate::macos_dock::apply_from_settings(&settings);
            // #MOC-62:开关当场变更即时生效 —— 开→切 Codex file 模式 + 同步镜像;
            // 关→删 config key 回退默认(`.credentials.json` 保留,非破坏)。
            if let Some(enabled) = portable_changed {
                let _ =
                    crate::admin::handlers::desktop::mcp_credentials_on_setting_changed(enabled);
            }
            // [MOC-100 P2-3] autoUnlockCodexPlugins 开关当场生效,无需重启 transfer:
            // 开→start daemon(幂等,已在跑则 no-op);关→stop daemon(gated,没跑则
            // no-op)。否则切到 false 后 daemon 还在跑、切回 true 又得重启才生效。
            // [MOC-104 relay] 开关 = 强制 CDP daemon 档。真实 chatgpt 活动走 relay 原生
            // 解锁、**不靠 daemon**(与 main.rs 自启决策一致:真实账号绝不启 daemon,免
            // MOC-100 高延迟)→ 这里**不再**因 active_is_real_chatgpt_now 而 start。
            // 开关开(强制档)→ 启 daemon;开关关 → 停(没跑则 no-op)。
            if let Some(enabled) = auto_unlock_changed {
                let service = crate::admin::handlers::plugin_unlock::get_service().await;
                if enabled {
                    service.start();
                } else {
                    service.stop().await;
                }
            }
            // MOC-144:webFetchBackend 改了 → 注册/移除 [mcp_servers.cat-webfetch]。
            // 失败不阻塞 settings 保存, 但回 webFetchSyncWarning 让前端 toast 提示用户
            // (MOC-145:现仅 error 日志, 用户当面无感)。Codex 需重启才重新加载 mcp_servers。
            let mut web_fetch_warning: Option<String> = None;
            if let Some(backend) = web_fetch_changed {
                if let Err(e) = crate::admin::services::mcp_servers::sync_web_fetch_server(&backend)
                {
                    // error 级: 用户当面改了设置但注册到 Codex 失败 = 会产生支持工单的静默
                    // 不一致;startup re-sync 会在下次 transfer 启动时幂等重试补偿。
                    tracing::error!("sync web_fetch mcp_server 失败(下次启动重试): {e}");
                    web_fetch_warning = Some(e);
                }
            }
            let mut body = json!({"success": true, "settings": settings});
            if let Some(w) = web_fetch_warning {
                body["webFetchSyncWarning"] = json!(w);
            }
            Json(body).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// [MOC-178] 真实账号模式持久开关(用户意图)。三态:键缺失=未设(走迁移)/ true=开 / false=
/// 用户主动关。**独立于活动 auth.json、不被退出 restore 撤销** —— 解决「清除后重启又自动开」。
/// 跟 `autoUnlockCodexPlugins`(无账号时的强制 CDP daemon 档)是两个独立开关。
pub fn read_real_account_mode_enabled() -> Option<bool> {
    crate::admin::registry_io::load().ok().and_then(|cfg| {
        cfg.get("settings")
            .and_then(|s| s.get("realAccountModeEnabled"))
            .and_then(Value::as_bool)
    })
}

/// [MOC-178] 写真实账号模式开关。toggle on / off / forget 都经这里落持久意图。
pub fn set_real_account_mode_enabled(enabled: bool) -> bool {
    with_config_write(|cfg| {
        ensure_settings_object(cfg)
            .insert("realAccountModeEnabled".to_owned(), Value::Bool(enabled));
        Ok(ConfigMutation::Modified(true))
    })
    .unwrap_or(false)
}

/// [MOC-257 三态] 插件解锁三态选择器持久值(用户意图):`"off"` / `"synthetic"` / `"real"`。
/// **键缺失 = 未手动设**,由 [`crate::codex_real_account::resolve_plugin_unlock_mode`] 按「本地有
/// 真账号 auth.json → real;否则 → synthetic」推导默认。取代旧的 `autoUnlockCodexPlugins`(CDP 档,
/// 已废弃)+ `fakeAccountModeEnabled` + `realAccountModeEnabled` 三个布尔开关。
pub fn read_plugin_unlock_mode() -> Option<String> {
    crate::admin::registry_io::load().ok().and_then(|cfg| {
        cfg.get("settings")
            .and_then(|s| s.get("pluginUnlockMode"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

/// [MOC-257 三态] 写三态选择器持久值。三态切换 handler 经这里落用户意图;`off` 是持久关闭意图,
/// 重启后仍把 auth.json 转移备份(见 codex_real_account stash)。
pub fn set_plugin_unlock_mode(mode: &str) -> bool {
    with_config_write(|cfg| {
        ensure_settings_object(cfg).insert(
            "pluginUnlockMode".to_owned(),
            Value::String(mode.to_owned()),
        );
        Ok(ConfigMutation::Modified(true))
    })
    .unwrap_or(false)
}

/// [MOC-257 review] 删 `pluginUnlockMode` 键(回到默认推导)。set 端点 apply 失败回滚用:若切换前本无
/// 键(走默认),回滚到无键而非某个固定值。键不存在 → no-op。成功返 true。
pub fn clear_plugin_unlock_mode() -> bool {
    with_config_write(|cfg| {
        if let Some(s) = cfg.get_mut("settings").and_then(Value::as_object_mut) {
            s.remove("pluginUnlockMode");
        }
        Ok(ConfigMutation::Modified(true))
    })
    .unwrap_or(false)
}

/// [MOC-257 三态] 一次性迁移:把旧三开关折叠成 `pluginUnlockMode`。键已存在 → no-op(幂等)。
/// 映射:`fakeAccountModeEnabled=true` → synthetic;`realAccountModeEnabled=true` → real;
/// **`realAccountModeEnabled=false`(显式关闭意图)→ off**(否则默认推导会从 deactivate 留下的
/// tokens 推成 real、撤销用户的关闭意图,review P2);`autoUnlockCodexPlugins=true`(旧 CDP 强制档,
/// 现由 synthetic 取代)→ synthetic;都没有 → **不写键**(留给默认推导:有真账号→real / 无→synthetic)。
pub fn migrate_plugin_unlock_mode_v1() -> bool {
    let cfg = match crate::admin::registry_io::load() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let s = cfg.get("settings");
    if s.and_then(|s| s.get("pluginUnlockMode")).is_some() {
        // [MOC-257 review] 已迁移(有 pluginUnlockMode),但**仍清残留 legacy autoUnlockCodexPlugins**:早期 build
        // 写了新键却没清旧键 / 导入的配置带旧键 → launcher `should_attach_debug_port`(process.rs)+ 设置热重载
        // 仍读它开 Codex 远程调试端口 / 旧 CDP 路径。存在则清掉(否则升级用户即便已迁移仍以调试端口启 Codex)。
        if s.and_then(|s| s.get("autoUnlockCodexPlugins")).is_some() {
            return with_config_write(|cfg| {
                if let Some(set) = cfg.get_mut("settings").and_then(Value::as_object_mut) {
                    set.remove("autoUnlockCodexPlugins");
                }
                Ok(ConfigMutation::Modified(true))
            })
            .unwrap_or(false);
        }
        return false; // 已迁移 + 无 legacy 残留
    }
    let b = |k: &str| {
        s.and_then(|s| s.get(k))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    // [MOC-257 review] 显式取真实账号 / 强制解锁键,区分「缺失」vs「Some(false)」—— 后者是用户主动关闭意图,
    // 不能跟缺失一样 fall through 到 None(会被 resolve 默认重新推成 synthetic/real)。
    let real_explicit = s
        .and_then(|s| s.get("realAccountModeEnabled"))
        .and_then(Value::as_bool);
    let auto_unlock = s
        .and_then(|s| s.get("autoUnlockCodexPlugins"))
        .and_then(Value::as_bool);
    let mode = if b("fakeAccountModeEnabled") {
        Some("synthetic")
    } else if real_explicit == Some(true) {
        Some("real")
    } else if real_explicit == Some(false) {
        // [MOC-257 review] 显式关真账号:开了 CDP 强制解锁仍要解锁 → synthetic(否则 off 会把插件关了,CDP
        // daemon 迁移后不再启);没开 → off(不被默认推导从残留 tokens 翻成 real)。
        Some(if auto_unlock == Some(true) {
            "synthetic"
        } else {
            "off"
        })
    } else if auto_unlock == Some(true) {
        // [MOC-257 review] 老 CDP 强制解锁、**无**显式真账号设置:有**可用真账号** → real(用真账号解锁,
        // 对齐缺键默认 resolve 的「有真账号→real」),否则 synthetic(无真账号也要解锁)。不能一律 synthetic,
        // 否则升级用户的可用真账号被 stash、真插件/backend 换成假空市场。
        Some(if crate::codex_real_account::real_account_usable() {
            "real"
        } else {
            "synthetic"
        })
    } else if auto_unlock == Some(false) {
        // [MOC-257 review] **显式关**强制解锁(且无其它显式模式)→ off。别跟缺键一样 fall through 到 None,
        // 否则 resolve 默认从残留 tokens 把用户已关的解锁重新推成 synthetic/real、启动又打开。
        Some("off")
    } else {
        None // autoUnlock 也缺 → 留给默认推导
    };
    match mode {
        Some(m) => {
            // [MOC-257 review] 设新三态 **+ 清掉 legacy autoUnlockCodexPlugins**:它迁移后仍被 launcher
            // `should_attach_debug_port` + settings 热重载消费(开 Codex 远程调试端口 / 旧 CDP 解锁路径),新 UI
            // 不再暴露 → 留 true 会让升级用户即便迁到 synthetic/real 仍以调试端口启 Codex。同写一次避免中间态。
            with_config_write(|cfg| {
                let s = ensure_settings_object(cfg);
                s.insert("pluginUnlockMode".to_owned(), Value::String(m.to_owned()));
                s.remove("autoUnlockCodexPlugins");
                Ok(ConfigMutation::Modified(true))
            })
            .unwrap_or(false)
        }
        None => false,
    }
}

/// #262:把 `settings.language` 同步到 adapters 全局 [`codex_app_transfer_adapters::core::language`]。
/// caller 路径:save_settings 后 + main.rs startup 加载 settings 时各调一次。
pub fn sync_user_language_from_settings(settings: &Value) {
    let lang = settings
        .get("language")
        .and_then(|v| v.as_str())
        .unwrap_or("en");
    codex_app_transfer_adapters::core::language::set_user_language(lang);
}

/// #262 startup helper:让 `main.rs setup` 不依赖私有 `load_registry` import,
/// 单独暴露一个跟 startup 路径绑定的 wrapper(失败 silent ok — language sync
/// 是 UI 偏好,不该 block 启动)。
pub fn load_registry_for_startup_language_sync() -> Result<Value, String> {
    load_registry()
}

// ── /api/config/* ────────────────────────────────────────────────────

pub async fn create_backup() -> impl IntoResponse {
    match create_config_backup("manual") {
        Ok(backup) => Json(json!({"success": true, "backup": backup})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn list_backups() -> impl IntoResponse {
    match list_config_backups() {
        Ok(backups) => Json(json!({"backups": backups})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub async fn export_config() -> impl IntoResponse {
    let cfg = load_registry().unwrap_or_else(|_| json!({}));
    Json(json!({
        "format": "codex-app-transfer.config",
        "exportedAt": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "config": cfg,
    }))
    .into_response()
}

pub async fn import_config(Json(data): Json<Value>) -> impl IntoResponse {
    let backup = match create_config_backup("before-import") {
        Ok(backup) => backup,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let normalized_base = match normalize_imported_config(&data) {
        Ok(config) => config,
        Err(e) => return err(StatusCode::BAD_REQUEST, e).into_response(),
    };
    // 用 with_config_write 包"读 current 用于保留 secret + 写 normalized" 整段,
    // 防 import 期间另一 RMW(eg form save / OAuth sync)读 current 看到旧
    // secret 又写回去导致 secret 状态不一致
    let result = with_config_write(|cfg| {
        let current = cfg.clone();
        let mut normalized = normalized_base.clone();
        preserve_existing_provider_secrets(&mut normalized, &current);
        *cfg = normalized;
        Ok(ConfigMutation::Modified(()))
    });
    if let Err(e) = result {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    // #262 Devin BUG-002 fix:import_config 替换整 config 也可能改 settings.language,
    // 必须同步 adapters 全局,否则 import 后 prompt 注入仍跟 import 前的语言一致
    // (silent failure)。失败 silent ok — language sync 是 UI 偏好不该 block import。
    if let Ok(reloaded) = load_registry() {
        let settings = reloaded
            .get("settings")
            .cloned()
            .unwrap_or_else(|| json!({}));
        sync_user_language_from_settings(&settings);
        // MOC-144:import 替换整 config 也可能改 webFetchBackend → 对齐
        // [mcp_servers.cat-webfetch] 注册态(与 save_settings 对称;否则 import 含 headless
        // 的配置后, 工具要到下次启动 re-sync 才注册)。sync 幂等, 无条件调即可(已一致不写)。
        let backend = settings
            .get("webFetchBackend")
            .and_then(|v| v.as_str())
            .unwrap_or(codex_app_transfer_registry::schema::DEFAULT_WEB_FETCH_BACKEND);
        if let Err(e) = crate::admin::services::mcp_servers::sync_web_fetch_server(backend) {
            tracing::error!("import 后 sync web_fetch mcp_server 失败(下次启动重试): {e}");
        }
    }
    Json(json!({
        "success": true,
        "message": "config imported",
        "backup": backup,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::response::IntoResponse;

    use super::super::common::test_support::with_isolated_home;

    fn config_with_secret() -> Value {
        json!({
            "version": APP_VERSION,
            "activeProvider": "p1",
            "gatewayApiKey": "cas_existing",
            "providers": [{
                "id": "p1",
                "name": "Provider One",
                "baseUrl": "https://api.example.com/v1",
                "authScheme": "bearer",
                "apiFormat": "openai_chat",
                "apiKey": "sk-existing",
                "extraHeaders": {"x-extra-secret": "secret-header"},
                "models": {"default": "model-one"},
                "sortIndex": 0
            }],
            "settings": {
                "theme": "default",
                "language": "zh",
                "proxyPort": 18080,
                "adminPort": 18081,
                "autoStart": false,
                "autoApplyOnStart": true,
                "exposeAllProviderModels": false,
                "restoreCodexOnExit": true,
                "updateUrl": DEFAULT_UPDATE_URL
            }
        })
    }

    #[test]
    fn config_backup_list_uses_real_files() {
        with_isolated_home(|home| {
            let cfg = config_with_secret();
            save_registry(&cfg).unwrap();

            let backup = create_config_backup("manual").unwrap();
            let name = backup.get("name").and_then(|v| v.as_str()).unwrap();
            assert!(name.starts_with("config-"));
            assert!(name.ends_with(".json"));
            assert!(backup.get("size").and_then(|v| v.as_u64()).unwrap() > 0);

            let backup_path = home.join(".codex-app-transfer").join("backups").join(name);
            assert!(backup_path.is_file());
            let saved: Value =
                serde_json::from_str(&fs::read_to_string(&backup_path).unwrap()).unwrap();
            assert_eq!(saved["providers"][0]["apiKey"], json!("sk-existing"));

            let backups = list_config_backups().unwrap();
            assert_eq!(backups.len(), 1);
            assert_eq!(backups[0]["name"], backup["name"]);
        });
    }

    #[test]
    fn import_config_backs_up_and_preserves_existing_provider_secrets_when_missing() {
        with_isolated_home(|_| {
            save_registry(&config_with_secret()).unwrap();

            let incoming = json!({
                "format": "codex-app-transfer.config",
                "config": {
                    "version": "1.0.3",
                    "activeProvider": "p1",
                    "gatewayApiKey": "cas_imported",
                    "providers": [{
                        "id": "p1",
                        "name": "Imported Provider",
                        "baseUrl": "https://imported.example.com/v1",
                        "authScheme": "bearer",
                        "apiFormat": "openai_chat",
                        "models": {"default": "imported-model"}
                    }],
                    "settings": {"proxyPort": 19090}
                }
            });

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let response = runtime.block_on(async { import_config(Json(incoming)).await });
            assert_eq!(response.into_response().status(), StatusCode::OK);

            let saved = load_registry().unwrap();
            assert_eq!(saved["activeProvider"], json!("p1"));
            assert_eq!(saved["gatewayApiKey"], json!("cas_imported"));
            assert_eq!(saved["settings"]["proxyPort"], json!(19090));
            assert_eq!(saved["providers"][0]["name"], json!("Imported Provider"));
            assert_eq!(saved["providers"][0]["apiKey"], json!("sk-existing"));
            assert_eq!(
                saved["providers"][0]["extraHeaders"]["x-extra-secret"],
                json!("secret-header")
            );

            let backups = list_config_backups().unwrap();
            assert_eq!(backups.len(), 1);
            assert!(backups[0]
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap()
                .contains("before-import"));
        });
    }
}
