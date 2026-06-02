use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use codex_app_transfer_codex_integration::{
    apply_provider, catalog_models_for_provider_with_display_names, ensure_file_store_mode,
    get_snapshot_status, has_snapshot, list_snapshots, read_auth, restore_available_count,
    restore_codex_snapshot, restore_codex_state, sync_mcp_credentials, ApplyConfig, CodexPaths,
};
use codex_app_transfer_gemini_oauth::antigravity_static_models;
use codex_app_transfer_proxy::proxy_telemetry;
use codex_app_transfer_registry::RawConfig;
use serde_json::{json, Value};

use crate::admin::handlers::common::{active_provider_name, read_setting_bool, APP_VERSION};
use crate::admin::handlers::providers::{
    active_provider, provider_api_key, provider_default_model, provider_display_name,
    provider_index, provider_model_capabilities, provider_model_mappings, provider_supports_1m,
};
use crate::admin::handlers::proxy::{
    ensure_gateway_key, read_gateway_key, read_proxy_port, start_proxy_if_needed,
};
use crate::admin::registry_io::{load as load_registry, with_config_write, ConfigMutation};
use crate::admin::state::AdminState;
use crate::proxy_runner::ProxyManager;

pub const ONE_M_CONTEXT_WINDOW: u64 = 1_000_000;

pub struct DesktopConfigTarget {
    pub base_url: String,
    pub api_key: String,
    pub supports_1m: bool,
    pub provider_name: String,
    pub default_model: String,
    pub model_mappings: Value,
    pub model_capabilities: Value,
    pub requires_proxy: bool,
    pub mode: &'static str,
    pub proxy_port: u16,
    /// #212:是否允许 Codex shell 工具网络访问(从 `Settings.codexNetworkAccess`
    /// 读取,默认 `true`)。写入 `sandbox_workspace_write.network_access`。
    pub codex_network_access: bool,
    /// #258:Codex Desktop 对话页底部 context 圆环 + tokens/s 默认显示开关
    /// (从 `Settings.codexStatusSectionDefaultVisible` 读取,默认 `true`)。
    /// 写入 `~/.codex/.codex-global-state.json` 的
    /// `electron-persisted-atom-state.local-conversation-status-section-visible`。
    pub codex_status_section_default_visible: bool,
    /// [MOC-69] model id → 人类可读 displayName(JSON object)。仅 antigravity 非空
    /// (从 static seed 构建);Codex Desktop model catalog 的 `display_name` 优先用它,
    /// 让 Codex 自己的 model picker 显示 displayName 而非 raw id。其他 provider 为
    /// `Value::Null`,catalog 回退 raw id(行为不变)。
    pub model_display_names: Value,
}

/// [MOC-69] 给 antigravity provider 构建 model id → displayName 反查表(JSON object),
/// 喂给 Codex model catalog 让其 picker 显示 displayName。数据源是 static seed
/// (`antigravity_static_models`,2026-05-30 实时上游刷新,已 SKIP 过滤),同步、无网络:
/// 避免在 config-apply 热路径(启动 / 切 provider / restore 都走)塞网络 I/O + 失败态。
/// 非 antigravity → `Value::Null`(catalog 回退 raw id)。
fn antigravity_display_names(api_format_lower: &str) -> Value {
    if !matches!(api_format_lower, "antigravity_oauth" | "antigravity") {
        return Value::Null;
    }
    let mut map = serde_json::Map::new();
    for m in antigravity_static_models() {
        if !m.display_name.is_empty() {
            map.insert(m.id, Value::String(m.display_name));
        }
    }
    Value::Object(map)
}

pub fn desktop_config_target_for_provider(
    cfg: &mut RawConfig,
    provider: &Value,
    proxy_port_override: Option<u16>,
) -> DesktopConfigTarget {
    let proxy_port = proxy_port_override.unwrap_or_else(|| read_proxy_port(cfg));

    // **bypass_proxy 模式**(2026-05-10):用户在「自定义第三方」preset 显式选
    // `apiFormat=responses` 协议,且填了 baseUrl + apiKey → Codex.app 直连上游,
    // 代理不参与转发(借鉴 codex-account-switch 的纯配置写入模式)。
    //
    // 适用范围:OpenAI 官方 / 任何原生实现 OpenAI Responses API 的反代或自建服务。
    // 触发条件:
    //   - apiFormat 严格等于 `responses` / `openai_responses`(anthropic /
    //     anthropic_messages / claude / messages 是 Anthropic Messages 路径
    //     → 继续走代理做本地协议转换)
    //   - baseUrl 与 apiKey 都非空(空了 direct 没法 work,fallback 到 local_proxy)
    //   - healing 命中 builtin preset 时强制覆盖 apiFormat=openai_chat,**builtin
    //     用户行为不变**(MiMo / Kimi / DeepSeek / MiniMax / 智谱 / 百炼 / Kimi Code 等)
    //
    // 历史教训(2026-05-08 MiMo Token Plan 404):v1.x 用 apiFormat=responses 当
    // "上游原生透传"隐式信号 → 用户配 MiMo 也被路由到 direct_provider → MiMo 上游
    // 没有 /responses端点 → 必 404。此次设计的关键差异:**healing 已经把所有
    // builtin preset 强制覆盖回 openai_chat**,bypass只可能命中显式自定义的
    // 第三方 provider —— 用户对此场景做出 informed choice。
    let api_format_lower = provider
        .get("apiFormat")
        .and_then(|v| v.as_str())
        .unwrap_or("openai_chat")
        .trim()
        .to_ascii_lowercase();
    let provider_base_url = provider
        .get("baseUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let direct_api_key = provider_api_key(provider);
    let bypass_proxy = matches!(api_format_lower.as_str(), "responses" | "openai_responses")
        && !provider_base_url.is_empty()
        && !direct_api_key.is_empty();

    let codex_network_access = crate::admin::handlers::proxy::read_codex_network_access(cfg);
    let codex_status_section_default_visible =
        crate::admin::handlers::proxy::read_codex_status_section_default_visible(cfg);

    if bypass_proxy {
        return DesktopConfigTarget {
            base_url: provider_base_url,
            api_key: direct_api_key,
            supports_1m: provider_supports_1m(provider),
            provider_name: provider_display_name(provider),
            default_model: provider_default_model(provider),
            model_mappings: provider_model_mappings(provider),
            model_capabilities: provider_model_capabilities(provider),
            requires_proxy: false,
            mode: "direct",
            proxy_port,
            codex_network_access,
            codex_status_section_default_visible,
            model_display_names: antigravity_display_names(&api_format_lower),
        };
    }

    // 默认 local_proxy 模式:Codex.app → 127.0.0.1:18080 → 本地代理(协议转换 +
    // extras 注入 + model 改写 + vision 剥离 + namespace MCP 展平等)→ 上游。
    // 本项目核心价值在协议转换层,默认所有 provider 走代理,需要透传必须显式选。
    let base_url = format!("http://127.0.0.1:{proxy_port}");
    // 熵源失败(getrandom)时 ensure_gateway_key 返 Err。desktop 配置生成属次要
    // 路径 —— proxy 端鉴权强制由 proxy_runner 启动路径的硬失败保证,这里降级为
    // 空 key + loud error log,绝不写入可预测的固定 key。
    let api_key = ensure_gateway_key(cfg).unwrap_or_else(|e| {
        tracing::error!(error_id = "GATEWAY_KEY_CSPRNG_FAILED", "{e}");
        String::new()
    });
    DesktopConfigTarget {
        base_url,
        api_key,
        supports_1m: provider_supports_1m(provider),
        provider_name: provider_display_name(provider),
        default_model: provider_default_model(provider),
        model_mappings: provider_model_mappings(provider),
        model_capabilities: provider_model_capabilities(provider),
        requires_proxy: true,
        mode: "local_proxy",
        proxy_port,
        codex_network_access,
        codex_status_section_default_visible,
        model_display_names: antigravity_display_names(&api_format_lower),
    }
}

pub fn desktop_target_for_active_provider(cfg: &RawConfig) -> Option<DesktopConfigTarget> {
    let provider = active_provider(cfg)?;
    let mut snapshot = cfg.clone();
    Some(desktop_config_target_for_provider(
        &mut snapshot,
        &provider,
        None,
    ))
}

pub fn desktop_expected_model_items(target: &DesktopConfigTarget) -> Vec<Value> {
    catalog_models_for_provider_with_display_names(
        &target.provider_name,
        &target.default_model,
        target.supports_1m,
        Some(&target.model_mappings),
        Some(&target.model_capabilities),
        Some(&target.model_display_names),
    )
    .into_iter()
    .map(|model| {
        let mut item = json!({
            "name": model.slug,
            "displayName": model.display_name,
        });
        if model.context_window >= ONE_M_CONTEXT_WINDOW {
            item["supports1m"] = Value::Bool(true);
        }
        item
    })
    .collect()
}

pub fn desktop_inference_models_json(target: Option<&DesktopConfigTarget>) -> String {
    let Some(target) = target else {
        return "[]".to_owned();
    };
    serde_json::to_string(&desktop_expected_model_items(target)).unwrap_or_else(|_| "[]".to_owned())
}

pub fn read_codex_toml_root_string(paths: &CodexPaths, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(&paths.config_toml).ok()?;
    for line in content.lines() {
        let stripped = line.trim_start();
        if stripped.starts_with('[') {
            break;
        }
        if !stripped.starts_with(key) {
            continue;
        }
        let after = &stripped[key.len()..];
        let mut rest = after.trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        rest = rest[1..].trim();
        if let Some(idx) = rest.find('#') {
            rest = rest[..idx].trim_end();
        }
        let trimmed = rest.trim_matches(|c: char| c == '"' || c == '\'');
        return Some(trimmed.to_owned());
    }
    None
}

pub fn codex_openai_api_key_present(paths: &CodexPaths) -> bool {
    read_auth(&paths.auth_json)
        .ok()
        .and_then(|auth| {
            auth.get("OPENAI_API_KEY")
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
        })
        .unwrap_or(false)
}

pub fn one_million_catalog_ready(paths: &CodexPaths, target: &DesktopConfigTarget) -> bool {
    // issue #317:direct 直连模式不写 model_catalog_json(用 Codex 默认 catalog),
    // 「1M catalog 是否写入」对 direct 无意义 —— 视为就绪。否则 desktop_health 会
    // 因 config.toml 没有 model_catalog_json 而永远返回 false → needsApply 死循环
    // (direct + default model 带 [1m] 后缀的 provider)。
    if !target.requires_proxy {
        return true;
    }
    let one_million_names: Vec<String> = desktop_expected_model_items(target)
        .into_iter()
        .filter_map(|item| {
            if item.get("supports1m").and_then(|v| v.as_bool()) == Some(true) {
                item.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned())
            } else {
                None
            }
        })
        .collect();
    if one_million_names.is_empty() {
        return true;
    }

    let Some(catalog_path) = read_codex_toml_root_string(paths, "model_catalog_json") else {
        return false;
    };
    let catalog_path = PathBuf::from(catalog_path);
    let telemetry = proxy_telemetry();
    let catalog: Value = match fs::read_to_string(&catalog_path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) => v,
            Err(e) => {
                telemetry.logs.add(
                    "WARN",
                    format!(
                        "one_million_catalog_ready: model_catalog JSON 解析失败 ({}): {e}",
                        catalog_path.display(),
                    ),
                );
                return false;
            }
        },
        Err(e) => {
            telemetry.logs.add(
                "WARN",
                format!(
                    "one_million_catalog_ready: model_catalog 文件读取失败 ({}): {e}",
                    catalog_path.display(),
                ),
            );
            return false;
        }
    };
    let Some(models) = catalog.get("models").and_then(|v| v.as_array()) else {
        return false;
    };
    models.iter().any(|item| {
        let slug = item
            .get("slug")
            .or_else(|| item.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !one_million_names.iter().any(|name| name == slug) {
            return false;
        }
        let context_window = item
            .get("context_window")
            .or_else(|| item.get("max_context_window"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        context_window >= ONE_M_CONTEXT_WINDOW
    })
}

pub fn desktop_health(
    paths: Option<&CodexPaths>,
    configured: bool,
    actual_base_url: Option<&str>,
    actual_api_key_present: bool,
    target: Option<&DesktopConfigTarget>,
) -> Value {
    let expected_base_url = target
        .map(|target| target.base_url.trim_end_matches('/').to_owned())
        .unwrap_or_default();
    let actual_base_url = actual_base_url
        .unwrap_or("")
        .trim()
        .trim_end_matches('/')
        .to_owned();
    let mut issues = Vec::new();

    if !configured {
        if !actual_base_url.is_empty() || actual_api_key_present {
            issues.push(json!({
                "code": "not_managed_by_cas",
                "message": "Current Codex CLI config was not written by the latest version of this tool.",
            }));
        } else {
            issues.push(json!({
                "code": "codex_snapshot_missing",
                "message": "Codex CLI config has not been applied by this tool — re-generate the config from the dashboard.",
            }));
        }
    }

    if !actual_base_url.is_empty()
        && !expected_base_url.is_empty()
        && actual_base_url != expected_base_url
    {
        issues.push(json!({
            "code": "gateway_base_url_mismatch",
            "message": "Codex CLI 仍指向旧地址，请重新一键生成 Codex CLI 配置。",
        }));
    }

    let one_million_ready = match (paths, target) {
        (Some(paths), Some(target)) => one_million_catalog_ready(paths, target),
        _ => true,
    };
    if !one_million_ready {
        issues.push(json!({
            "code": "one_million_not_written",
            "message": "1M 上下文模型尚未写入 Codex CLI 配置，请重新一键生成配置并重启终端。",
        }));
    }

    json!({
        "needsApply": !configured || !issues.is_empty(),
        "oneMillionReady": one_million_ready,
        "expectedBaseUrl": expected_base_url,
        "actualBaseUrl": actual_base_url,
        "mode": target.map(|target| target.mode),
        "requiresProxy": target.map(|target| target.requires_proxy).unwrap_or(false),
        "issues": issues,
    })
}

pub fn apply_desktop_target(target: &DesktopConfigTarget) -> Result<Value, String> {
    let paths = CodexPaths::from_home_env().map_err(|e| e.to_string())?;
    // [MOC-104] relay 模式 gate:仅 proxy 模式(非 direct)+ 活动已是可用真实
    // chatgpt 时,apply 保留 chatgpt 登录态(让 Codex 原生显示 Plugins 入口、
    // 不再依赖 CDP daemon 注入,消除 MOC-100 高延迟)。direct 直连 bypass proxy、
    // 凭据直接用 auth.json 发上游,保留 chatgpt token 直连第三方会 401 → 不 relay。
    let preserve_chatgpt_auth =
        target.mode != "direct" && crate::codex_real_account::active_is_real_chatgpt_now();
    let result = apply_provider(
        &paths,
        &ApplyConfig {
            base_url: &target.base_url,
            gateway_api_key: &target.api_key,
            supports_1m: target.supports_1m,
            provider_name: &target.provider_name,
            default_model: &target.default_model,
            model_mappings: Some(&target.model_mappings),
            model_capabilities: Some(&target.model_capabilities),
            model_display_names: Some(&target.model_display_names),
            app_version: APP_VERSION,
            codex_network_access: target.codex_network_access,
            codex_status_section_default_visible: target.codex_status_section_default_visible,
            // issue #317:direct 直连只写上游配置,strip 全部 transfer 私货。
            direct: target.mode == "direct",
            preserve_chatgpt_auth,
        },
    )
    .map_err(|e| format!("apply 失败: {e}"))?;
    serde_json::to_value(result).map_err(|e| format!("apply 结果序列化失败: {e}"))
}

pub async fn sync_desktop_for_active_provider(state: &AdminState) -> Value {
    let target_result = with_config_write(|cfg| {
        let Some(provider) = active_provider(cfg) else {
            return Err("no default provider".into());
        };
        let target = desktop_config_target_for_provider(cfg, &provider, None);
        Ok(ConfigMutation::Modified(target))
    });
    let target = match target_result {
        Ok(t) => t,
        Err(e) if e == "no default provider" => {
            return json!({
                "attempted": false,
                "success": false,
                "message": e,
            });
        }
        Err(e) => return json!({"attempted": true, "success": false, "message": e}),
    };

    let mut proxy_started = false;
    if target.requires_proxy {
        match start_proxy_if_needed(&state.proxy_manager, target.proxy_port).await {
            Ok(started) => proxy_started = started,
            Err(e) => {
                return json!({"attempted": true, "success": false, "mode": target.mode, "requiresProxy": target.requires_proxy, "message": e});
            }
        }
    } else {
        state.proxy_manager.stop_silent();
    }

    match apply_desktop_target(&target) {
        Ok(mut result) => {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("attempted".into(), Value::Bool(true));
                obj.insert("success".into(), Value::Bool(true));
                obj.insert("mode".into(), Value::String(target.mode.to_owned()));
                obj.insert("requiresProxy".into(), Value::Bool(target.requires_proxy));
                obj.insert("proxyStarted".into(), Value::Bool(proxy_started));
            }
            result
        }
        Err(e) => {
            json!({"attempted": true, "success": false, "mode": target.mode, "requiresProxy": target.requires_proxy, "proxyStarted": proxy_started, "message": e})
        }
    }
}

pub async fn auto_apply_on_startup_if_enabled(proxy_manager: Arc<ProxyManager>) -> Value {
    let cfg = match load_registry() {
        Ok(cfg) => cfg,
        Err(e) => {
            return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": format!("failed: {e}")})
        }
    };
    if !read_setting_bool(&cfg, "autoApplyOnStart", true) {
        return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": "disabled by settings"});
    }
    if active_provider(&cfg).is_none() {
        return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": "no active provider; skip"});
    }
    let state = AdminState { proxy_manager };
    let result = sync_desktop_for_active_provider(&state).await;
    if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        return json!({
            "applied": true,
            "requiresProxy": result.get("requiresProxy").and_then(|v| v.as_bool()).unwrap_or(false),
            "proxyStarted": result.get("proxyStarted").and_then(|v| v.as_bool()).unwrap_or(false),
            "message": format!("applied {}", active_provider_name(&cfg)),
        });
    }
    json!({
        "applied": false,
        "requiresProxy": result.get("requiresProxy").and_then(|v| v.as_bool()).unwrap_or(false),
        "proxyStarted": result.get("proxyStarted").and_then(|v| v.as_bool()).unwrap_or(false),
        "message": format!("failed: {}", result.get("message").and_then(|v| v.as_str()).unwrap_or("unknown")),
    })
}

pub fn restore_codex_if_enabled(reason: &str) -> Value {
    let cfg = match load_registry() {
        Ok(cfg) => cfg,
        Err(e) => {
            return json!({"attempted": true, "restored": false, "success": false, "reason": reason, "message": e})
        }
    };
    if !read_setting_bool(&cfg, "restoreCodexOnExit", true) {
        return json!({"attempted": false, "restored": false, "success": true, "reason": reason, "message": "disabled by settings"});
    }
    let paths = match CodexPaths::from_home_env() {
        Ok(paths) => paths,
        Err(e) => {
            return json!({"attempted": true, "restored": false, "success": false, "reason": reason, "message": e.to_string()})
        }
    };
    if !has_snapshot(&paths) {
        return json!({"attempted": false, "restored": false, "success": true, "reason": reason, "message": "no snapshot; skip"});
    }
    match restore_codex_state(&paths) {
        Ok(restored) => {
            json!({"attempted": true, "restored": restored, "success": true, "reason": reason})
        }
        Err(e) => {
            json!({"attempted": true, "restored": false, "success": false, "reason": reason, "message": e.to_string()})
        }
    }
}

pub async fn switch_provider_and_sync(
    proxy_manager: Arc<ProxyManager>,
    provider_id: String,
) -> Value {
    let result = with_config_write(|cfg| {
        if provider_index(cfg, &provider_id).is_none() {
            return Err("provider not found".into());
        }
        cfg.as_object_mut()
            .unwrap()
            .insert("activeProvider".into(), Value::String(provider_id.clone()));
        Ok(ConfigMutation::Modified(()))
    });
    if let Err(e) = result {
        return json!({"success": false, "message": e});
    }
    let state = AdminState { proxy_manager };
    let desktop_sync = sync_desktop_for_active_provider(&state).await;
    // MOC-62:切换后做一次 MCP 凭据并集同步(capture 新授权 + 必要时 restore;
    // 只动两个凭据文件,不碰 config.toml)。
    mcp_credentials_capture_after_switch();
    json!({
        "success": true,
        "message": "default provider updated",
        "desktopSync": desktop_sync,
    })
}

/// MOC-62:启动时按 `mcpCredentialsPortableStore` 开关(默认开)把 Codex 切到 file
/// 存储 + 同步 transfer 镜像,返回 `restore_available`(live 整文件缺失 + 镜像有 N 条 →
/// >0 时由 `main.rs` emit 事件让前端弹"从备份恢复?"确认)。**关闭时启动不动** ——
/// 关闭的回退只在用户当场切关时由 [`mcp_credentials_on_setting_changed`] 跑一次,避免
/// 每次启动都去删 key 误伤用户手设的 `keyring`。调用点在 `main.rs` 的 post-apply task
/// 末尾(已 await auto_apply,config.toml 写已落定 → 不与 apply 抢写)。
pub fn mcp_credentials_startup_sync(reason: &str) -> usize {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(reason, "mcp-cred: load_registry failed: {e}");
            return 0;
        }
    };
    if !read_setting_bool(&cfg, "mcpCredentialsPortableStore", true) {
        return 0;
    }
    apply_mcp_portable_store(true, reason)
}

/// MOC-62:用户在设置页切 `mcpCredentialsPortableStore` 时调用。开→切 file 模式 +
/// 同步;关→删 config key 回退 Codex 默认(`.credentials.json` **保留**,非破坏)。
pub fn mcp_credentials_on_setting_changed(enabled: bool) -> usize {
    apply_mcp_portable_store(enabled, "setting-changed")
}

/// MOC-62:只读查询当前"是否有可恢复的 MCP 凭据备份"(开关开 + live 整文件缺失 + 镜像
/// 非空 → 返回可恢复条数)。前端 load 时轮询此状态决定是否弹恢复确认 —— 比一次性
/// startup event 可靠(避免 event 在前端 listener 注册前就 emit 而丢失,
/// chatgpt-codex-connector P2)。无副作用。
pub fn mcp_credentials_restore_status() -> usize {
    let Ok(cfg) = load_registry() else {
        return 0;
    };
    if !read_setting_bool(&cfg, "mcpCredentialsPortableStore", true) {
        return 0;
    }
    match CodexPaths::from_home_env() {
        Ok(paths) => restore_available_count(&paths),
        Err(_) => 0,
    }
}

/// 返回 `restore_available`(>0 = live 整文件缺失且镜像有备份,需弹恢复确认);
/// disabled / error / 无可恢复都返回 0。
fn apply_mcp_portable_store(enabled: bool, reason: &str) -> usize {
    let paths = match CodexPaths::from_home_env() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, reason, "mcp-cred: CodexPaths::from_home_env failed");
            return 0;
        }
    };
    if let Err(e) = ensure_file_store_mode(&paths, enabled) {
        tracing::warn!(error = %e, reason, enabled, "mcp-cred: ensure_file_store_mode failed");
        return 0;
    }
    if !enabled {
        tracing::info!(
            reason,
            "mcp-cred: portable store disabled, config key reverted (file kept)"
        );
        return 0;
    }
    match sync_mcp_credentials(&paths) {
        Ok(rep) => {
            tracing::info!(
                reason,
                captured = rep.captured,
                dropped = rep.dropped,
                mirror_written = rep.mirror_written,
                restore_available = rep.restore_available,
                skipped = ?rep.skipped,
                "mcp-cred: portable store synced"
            );
            rep.restore_available
        }
        Err(e) => {
            tracing::warn!(error = %e, reason, "mcp-cred: sync failed");
            0
        }
    }
}

/// MOC-62:provider 交互式切换后做一次 MCP 凭据并集同步(仅开关开时)。本质是完整的
/// `sync_mcp_credentials`(capture 新授权进镜像 + 必要时从镜像 restore 回 live),只动
/// 两个凭据文件、**不碰 config.toml**。缩短"新授权还没进镜像就被外部擦掉"的窗口。
fn mcp_credentials_capture_after_switch() {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(_) => return,
    };
    if !read_setting_bool(&cfg, "mcpCredentialsPortableStore", true) {
        return;
    }
    if let Ok(paths) = CodexPaths::from_home_env() {
        match sync_mcp_credentials(&paths) {
            Ok(rep) => tracing::info!(
                captured = rep.captured,
                dropped = rep.dropped,
                restore_available = rep.restore_available,
                "mcp-cred: sync after provider switch"
            ),
            Err(e) => tracing::warn!(error = %e, "mcp-cred: capture after switch failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::admin::handlers::common::test_support::with_isolated_home;
    use crate::admin::handlers::desktop::desktop_status;
    use crate::admin::registry_io::save_for_test as save_registry;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use codex_app_transfer_registry::DEFAULT_UPDATE_URL;

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

    fn agent_debug_log(hypothesis_id: &str, location: &str, message: &str, data: Value) {
        let payload = json!({
            "sessionId": "bf3f9f",
            "runId": "pre-fix",
            "hypothesisId": hypothesis_id,
            "location": location,
            "message": message,
            "data": data,
            "timestamp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/Users/alysechen/alysechen/github/codex-app-transfer/.cursor/debug-bf3f9f.log")
        {
            let _ = writeln!(f, "{}", payload);
        }
    }

    #[test]
    fn desktop_config_target_matches_legacy_proxy_and_direct_modes() {
        let mut proxy_cfg = config_with_secret();
        proxy_cfg["gatewayApiKey"] = Value::Null;
        let proxy_provider = active_provider(&proxy_cfg).unwrap();
        let proxy_target =
            desktop_config_target_for_provider(&mut proxy_cfg, &proxy_provider, Some(19090));
        assert_eq!(proxy_target.mode, "local_proxy");
        assert!(proxy_target.requires_proxy);
        assert_eq!(proxy_target.base_url, "http://127.0.0.1:19090");
        assert!(proxy_target.api_key.starts_with("cas_"));
        assert_eq!(
            proxy_cfg
                .get("gatewayApiKey")
                .and_then(|v| v.as_str())
                .unwrap(),
            proxy_target.api_key
        );

        let mut direct_cfg = config_with_secret();
        direct_cfg["gatewayApiKey"] = Value::Null;
        let direct_provider = json!({
            "id": "custom-third-party-instance",
            "name": "Custom Third-Party (Direct)",
            "baseUrl": "https://direct.example.com/v1/",
            "authScheme": "bearer",
            "apiFormat": "responses",
            "apiKey": "sk-direct",
            "models": {"default": "direct-model"},
        });
        let target =
            desktop_config_target_for_provider(&mut direct_cfg, &direct_provider, Some(19090));
        assert_eq!(
            target.mode, "direct",
            "apiFormat=responses + 自定义第三方 + 填齐 baseUrl/apiKey → direct 透传"
        );
        assert!(!target.requires_proxy, "direct 模式不启动本地代理");
        assert_eq!(
            target.base_url, "https://direct.example.com/v1/",
            "config.toml 直接指向用户填的上游 baseUrl"
        );
        assert_eq!(
            target.api_key, "sk-direct",
            "auth.json 直接写用户填的 apiKey"
        );
        assert!(direct_cfg
            .get("gatewayApiKey")
            .map(|v| v.is_null())
            .unwrap_or(false));
    }

    #[test]
    fn responses_format_without_apikey_falls_back_to_local_proxy() {
        let mut cfg = config_with_secret();
        let provider = json!({
            "id": "incomplete-direct",
            "name": "Incomplete Direct",
            "baseUrl": "https://direct.example.com/v1/",
            "authScheme": "bearer",
            "apiFormat": "responses",
            "apiKey": "",
            "models": {"default": "x"},
        });
        let target = desktop_config_target_for_provider(&mut cfg, &provider, Some(19090));
        assert_eq!(target.mode, "local_proxy", "apiKey 空时 fallback");
        assert!(target.requires_proxy);
    }

    #[test]
    fn responses_format_without_baseurl_falls_back_to_local_proxy() {
        let mut cfg = config_with_secret();
        let provider = json!({
            "id": "incomplete-direct-2",
            "name": "Incomplete Direct 2",
            "baseUrl": "",
            "authScheme": "bearer",
            "apiFormat": "responses",
            "apiKey": "sk-x",
            "models": {"default": "x"},
        });
        let target = desktop_config_target_for_provider(&mut cfg, &provider, Some(19090));
        assert_eq!(target.mode, "local_proxy");
        assert!(target.requires_proxy);
    }

    #[test]
    fn anthropic_aliases_never_bypass_proxy() {
        for fmt in [
            "anthropic",
            "anthropic_messages",
            "claude",
            "messages",
            "claude_messages",
        ] {
            let mut cfg = config_with_secret();
            let provider = json!({
                "id": "anthropic-aliased",
                "name": "Anthropic Aliased",
                "baseUrl": "https://anthropic-style.example.com/v1/",
                "authScheme": "bearer",
                "apiFormat": fmt,
                "apiKey": "sk-x",
                "models": {"default": "claude-sonnet"},
            });
            let target = desktop_config_target_for_provider(&mut cfg, &provider, Some(19090));
            assert_eq!(
                target.mode, "local_proxy",
                "{fmt} 必须走代理协议转换,不能进 bypass"
            );
            assert!(target.requires_proxy, "{fmt} 必须 requires_proxy=true");
        }
    }

    #[test]
    fn openai_responses_alias_triggers_direct_mode() {
        let mut cfg = config_with_secret();
        let provider = json!({
            "id": "alias-direct",
            "name": "Alias Direct",
            "baseUrl": "https://api.openai.com/v1/",
            "authScheme": "bearer",
            "apiFormat": "openai_responses",
            "apiKey": "sk-direct",
            "models": {"default": "gpt-5"},
        });
        let target = desktop_config_target_for_provider(&mut cfg, &provider, Some(19090));
        assert_eq!(
            target.mode, "direct",
            "openai_responses 别名必须跟 responses 同样进 bypass"
        );
        assert!(!target.requires_proxy);
        assert_eq!(target.base_url, "https://api.openai.com/v1/");
        assert_eq!(target.api_key, "sk-direct");
    }

    #[test]
    fn switch_back_to_builtin_restarts_proxy_and_repoints_config() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|home| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["settings"]["proxyPort"] = json!(0);
                cfg["providers"] = json!([
                    cfg["providers"][0].clone(),
                    {
                        "id": "p2",
                        "name": "Custom Direct",
                        "baseUrl": "https://direct.example.com/v1/",
                        "authScheme": "bearer",
                        "apiFormat": "responses",
                        "apiKey": "sk-direct",
                        "models": {"default": "direct-model"},
                        "sortIndex": 1
                    }
                ]);
                save_registry(&cfg).unwrap();
                fs::create_dir_all(home.join(".codex")).unwrap();

                let manager = Arc::new(ProxyManager::new());

                let r1 = switch_provider_and_sync(Arc::clone(&manager), "p2".to_owned()).await;
                assert_eq!(r1["desktopSync"]["mode"], json!("direct"));
                assert!(!manager.status().running, "direct 模式不启动代理");
                let toml1 = fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
                assert!(toml1.contains("direct.example.com"));

                let p1_id = cfg["providers"][0]["id"].as_str().unwrap().to_owned();
                let r2 = switch_provider_and_sync(Arc::clone(&manager), p1_id).await;
                assert_eq!(r2["desktopSync"]["mode"], json!("local_proxy"));
                assert!(manager.status().running, "切回 builtin 必须重启代理");
                let toml2 = fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
                assert!(
                    toml2.contains("openai_base_url = \"http://127.0.0.1:"),
                    "config.toml 必须重新指向 127.0.0.1,实际:\n{toml2}"
                );
                assert!(
                    !toml2.contains("direct.example.com"),
                    "禁止残留 direct 上游 URL:\n{toml2}"
                );
                let auth_json: Value = serde_json::from_str(
                    &fs::read_to_string(home.join(".codex").join("auth.json")).unwrap(),
                )
                .unwrap();
                let api_key = auth_json["OPENAI_API_KEY"].as_str().unwrap_or_default();
                assert_ne!(
                    api_key, "sk-direct",
                    "auth.json 不能残留 direct 时的 provider apiKey"
                );
                assert!(
                    !api_key.is_empty(),
                    "auth.json 必须有 gateway key(local_proxy 模式)"
                );

                manager.stop_silent();
            });
        });
    }

    #[test]
    fn startup_auto_apply_starts_proxy_and_exit_restore_uses_snapshot() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|home| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["gatewayApiKey"] = Value::Null;
                cfg["settings"]["proxyPort"] = json!(0);
                save_registry(&cfg).unwrap();

                let codex_dir = home.join(".codex");
                fs::create_dir_all(&codex_dir).unwrap();
                let config_toml = codex_dir.join("config.toml");
                fs::write(&config_toml, "approval_policy = \"on-request\"\n").unwrap();

                let manager = Arc::new(ProxyManager::new());
                let result = auto_apply_on_startup_if_enabled(Arc::clone(&manager)).await;
                assert_eq!(result["applied"], json!(true));
                assert_eq!(result["requiresProxy"], json!(true));
                assert_eq!(result["proxyStarted"], json!(true));
                assert!(manager.status().running);

                let saved = load_registry().unwrap();
                assert!(saved
                    .get("gatewayApiKey")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .starts_with("cas_"));
                let paths = CodexPaths::from_home_env().unwrap();
                assert!(has_snapshot(&paths));
                let applied_config = fs::read_to_string(&config_toml).unwrap();
                assert!(applied_config.contains("approval_policy = \"never\""));
                assert!(applied_config.contains("sandbox_mode = \"danger-full-access\""));
                assert!(applied_config.contains("openai_base_url = \"http://127.0.0.1:0\""));

                let restored = restore_codex_if_enabled("test-exit");
                assert_eq!(restored["success"], json!(true));
                assert_eq!(restored["attempted"], json!(true));
                assert!(!has_snapshot(&paths));
                let restored_config = fs::read_to_string(&config_toml).unwrap();
                assert!(restored_config.contains("approval_policy = \"on-request\""));
                assert!(!restored_config.contains("sandbox_mode"));
                assert!(!restored_config.contains("openai_base_url"));
                manager.stop_silent();
            });
        });
    }

    #[test]
    fn startup_auto_apply_respects_disabled_setting() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|_| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["settings"]["autoApplyOnStart"] = json!(false);
                save_registry(&cfg).unwrap();

                let manager = Arc::new(ProxyManager::new());
                let result = auto_apply_on_startup_if_enabled(Arc::clone(&manager)).await;
                assert_eq!(result["applied"], json!(false));
                assert_eq!(result["message"], json!("disabled by settings"));
                assert!(!manager.status().running);
            });
        });
    }

    #[test]
    fn provider_switch_syncs_desktop_via_direct_when_apiformat_is_responses() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|home| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["settings"]["proxyPort"] = json!(0);
                cfg["providers"] = json!([
                    cfg["providers"][0].clone(),
                    {
                        "id": "p2",
                        "name": "Custom Third-Party (Direct)",
                        "baseUrl": "https://direct.example.com/v1/",
                        "authScheme": "bearer",
                        "apiFormat": "responses",
                        "apiKey": "sk-direct",
                        "models": {"default": "direct-model"},
                        "sortIndex": 1
                    }
                ]);
                save_registry(&cfg).unwrap();
                fs::create_dir_all(home.join(".codex")).unwrap();

                let manager = Arc::new(ProxyManager::new());
                let result = switch_provider_and_sync(Arc::clone(&manager), "p2".to_owned()).await;

                assert_eq!(result["success"], json!(true));
                assert_eq!(result["desktopSync"]["success"], json!(true));
                assert_eq!(result["desktopSync"]["mode"], json!("direct"));
                assert_eq!(result["desktopSync"]["requiresProxy"], json!(false));
                assert!(
                    !manager.status().running,
                    "direct 模式必须不启动代理(stop_silent)"
                );

                let saved = load_registry().unwrap();
                assert_eq!(saved["activeProvider"], json!("p2"));
                let config_toml =
                    fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
                assert!(
                    config_toml.contains("openai_base_url = \"https://direct.example.com/v1/\""),
                    "config.toml 必须指向用户填的上游 baseUrl,实际:\n{config_toml}"
                );
                assert!(
                    !config_toml.contains("127.0.0.1"),
                    "direct 模式禁止指向 127.0.0.1:\n{config_toml}"
                );
                let auth_json: Value = serde_json::from_str(
                    &fs::read_to_string(home.join(".codex").join("auth.json")).unwrap(),
                )
                .unwrap();
                let api_key = auth_json["OPENAI_API_KEY"].as_str().unwrap_or_default();
                assert_eq!(
                    api_key, "sk-direct",
                    "auth.json 必须写用户填的 provider apiKey"
                );
            });
        });
    }

    #[test]
    fn desktop_inference_models_use_current_codex_catalog_slots() {
        let mut cfg = config_with_secret();
        cfg["providers"][0]["models"] = json!({
            "default": "deepseek-v4-pro[1m]",
            "gpt_5_5": "kimi-k2",
            "gpt_5_4": "glm-4.6",
        });
        let provider = active_provider(&cfg).unwrap();
        let target = desktop_config_target_for_provider(&mut cfg, &provider, Some(19090));
        let raw = desktop_inference_models_json(Some(&target));

        assert!(!raw.contains("sonnet"));
        assert!(!raw.contains("haiku"));
        assert!(!raw.contains("opus"));

        let models: Vec<Value> = serde_json::from_str(&raw).unwrap();
        let names: Vec<&str> = models
            .iter()
            .filter_map(|item| item.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"gpt-5.5"));
        assert!(names.contains(&"gpt-5.4"));
        assert!(names.contains(&"gpt-5.4-mini"));
        assert!(names.contains(&"deepseek-v4-pro"));
        assert!(models
            .iter()
            .any(|item| item.get("supports1m").and_then(|v| v.as_bool()) == Some(true)));
    }

    #[test]
    fn desktop_status_reports_current_models_and_health_issues() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|home| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["providers"][0]["models"] = json!({
                    "default": "deepseek-v4-pro[1m]",
                    "gpt_5_5": "kimi-k2",
                });
                save_registry(&cfg).unwrap();

                let codex_dir = home.join(".codex");
                fs::create_dir_all(&codex_dir).unwrap();
                fs::write(
                    codex_dir.join("config.toml"),
                    "openai_base_url = \"http://127.0.0.1:18080\"\n",
                )
                .unwrap();
                fs::write(
                    codex_dir.join("auth.json"),
                    "{\"OPENAI_API_KEY\":\"cas_existing\"}\n",
                )
                .unwrap();

                let response = desktop_status().await.into_response();
                assert_eq!(response.status(), StatusCode::OK);
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let payload: Value = serde_json::from_slice(&body).unwrap();

                let models_raw = payload["keys"]["inferenceModels"].as_str().unwrap();
                assert!(!models_raw.contains("sonnet"));
                assert!(models_raw.contains("gpt-5.5"));
                assert!(models_raw.contains("deepseek-v4-pro"));
                assert_eq!(payload["configured"], json!(false));
                assert_eq!(payload["health"]["needsApply"], json!(true));
                assert_eq!(payload["health"]["oneMillionReady"], json!(false));

                let codes: Vec<&str> = payload["health"]["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|issue| issue.get("code").and_then(|v| v.as_str()))
                    .collect();
                assert!(codes.contains(&"not_managed_by_cas"));
                assert!(codes.contains(&"one_million_not_written"));
            });
        });
    }

    #[test]
    fn desktop_health_reports_base_url_mismatch() {
        with_isolated_home(|home| {
            let cfg = config_with_secret();
            let provider = active_provider(&cfg).unwrap();
            let mut target_cfg = cfg.clone();
            let target =
                desktop_config_target_for_provider(&mut target_cfg, &provider, Some(19090));

            let codex_dir = home.join(".codex");
            fs::create_dir_all(&codex_dir).unwrap();
            fs::write(
                codex_dir.join("config.toml"),
                "openai_base_url = \"http://127.0.0.1:18080\"\n",
            )
            .unwrap();
            fs::write(
                codex_dir.join("auth.json"),
                "{\"OPENAI_API_KEY\":\"cas_old\"}\n",
            )
            .unwrap();

            let paths = CodexPaths::from_home_env().unwrap();
            let actual_base_url = read_codex_toml_root_string(&paths, "openai_base_url");
            let health = desktop_health(
                Some(&paths),
                false,
                actual_base_url.as_deref(),
                true,
                Some(&target),
            );

            assert_eq!(health["needsApply"], json!(true));
            assert_eq!(health["expectedBaseUrl"], json!("http://127.0.0.1:19090"));
            assert_eq!(health["actualBaseUrl"], json!("http://127.0.0.1:18080"));
            let codes: Vec<&str> = health["issues"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|issue| issue.get("code").and_then(|v| v.as_str()))
                .collect();
            assert!(codes.contains(&"not_managed_by_cas"));
            assert!(codes.contains(&"gateway_base_url_mismatch"));
        });
    }

    #[test]
    fn qwen_local_proxy_apply_writes_codex_auth_and_base_url() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        with_isolated_home(|home| {
            runtime.block_on(async {
                let mut cfg = config_with_secret();
                cfg["activeProvider"] = json!("qwen");
                cfg["gatewayApiKey"] = Value::Null;
                cfg["settings"]["proxyPort"] = json!(19091);
                cfg["providers"] = json!([{
                    "id": "qwen",
                    "name": "Qwen",
                    "baseUrl": "https://dashscope.aliyuncs.com/compatible-mode/v1",
                    "authScheme": "bearer",
                    "apiFormat": "openai_chat",
                    "apiKey": "sk-qwen-upstream",
                    "models": {"default": "qwen3.6-plus"},
                    "sortIndex": 0
                }]);
                save_registry(&cfg).unwrap();
                fs::create_dir_all(home.join(".codex")).unwrap();

                agent_debug_log(
                    "H1",
                    "src-tauri/src/admin/handlers/desktop.rs:qwen_local_proxy_apply_writes_codex_auth_and_base_url:before_sync",
                    "prepared qwen provider config",
                    json!({
                        "activeProvider": "qwen",
                        "proxyPort": 19091,
                        "providerModel": "qwen3.6-plus",
                        "gatewayApiKeyNull": true
                    }),
                );

                let manager = Arc::new(ProxyManager::new());
                let result = switch_provider_and_sync(Arc::clone(&manager), "qwen".to_owned()).await;

                agent_debug_log(
                    "H1",
                    "src-tauri/src/admin/handlers/desktop.rs:qwen_local_proxy_apply_writes_codex_auth_and_base_url:after_sync",
                    "desktop sync result for qwen",
                    json!({
                        "success": result["success"],
                        "desktopSyncSuccess": result["desktopSync"]["success"],
                        "desktopMode": result["desktopSync"]["mode"],
                        "requiresProxy": result["desktopSync"]["requiresProxy"],
                    }),
                );

                assert_eq!(result["success"], json!(true));
                assert_eq!(result["desktopSync"]["success"], json!(true));
                assert_eq!(result["desktopSync"]["mode"], json!("local_proxy"));
                assert_eq!(result["desktopSync"]["requiresProxy"], json!(true));

                let toml = fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
                let auth_raw = fs::read_to_string(home.join(".codex").join("auth.json")).unwrap();
                let auth_json: Value = serde_json::from_str(&auth_raw).unwrap();
                let injected_key = auth_json["OPENAI_API_KEY"].as_str().unwrap_or_default();

                agent_debug_log(
                    "H2",
                    "src-tauri/src/admin/handlers/desktop.rs:qwen_local_proxy_apply_writes_codex_auth_and_base_url:codex_files",
                    "verified codex files after qwen apply",
                    json!({
                        "tomlHasProxyBaseUrl": toml.contains("openai_base_url = \"http://127.0.0.1:19091\""),
                        "tomlHas1m": toml.contains("model_context_window = 1000000"),
                        "authMode": auth_json["auth_mode"],
                        "injectedKeyPrefix": injected_key.get(0..4).unwrap_or(""),
                        "injectedKeyLen": injected_key.len(),
                    }),
                );

                assert!(toml.contains("openai_base_url = \"http://127.0.0.1:19091\""));
                assert!(
                    toml.contains("model_context_window = 1000000"),
                    "qwen3.6-* should be treated as 1M-capable"
                );
                assert_eq!(auth_json["auth_mode"], json!("apikey"));
                assert!(
                    injected_key.starts_with("cas_") && !injected_key.is_empty(),
                    "qwen local_proxy should inject gateway key into auth.json"
                );
                manager.stop_silent();
            });
        });
    }

    /// issue #317 回归保护:direct(`requires_proxy=false`)不写 model_catalog_json,
    /// `one_million_catalog_ready` 必须直接判就绪(short-circuit),不读 catalog
    /// 文件。否则 direct + default model 带 [1m] 的 provider 会让 desktop_health
    /// 永远 needsApply=true。短路在读文件之前,即便 paths 指向不存在目录也成立。
    #[test]
    fn one_million_catalog_ready_short_circuits_true_for_direct() {
        let paths = CodexPaths::from_home_dir(std::path::Path::new("/nonexistent-cas-317"));
        let direct_target = DesktopConfigTarget {
            base_url: "https://up.example.com/v1".into(),
            api_key: "sk".into(),
            supports_1m: true,
            provider_name: "Custom".into(),
            default_model: "gpt-5.5[1m]".into(),
            model_mappings: Value::Null,
            model_capabilities: Value::Null,
            model_display_names: serde_json::Value::Null,
            requires_proxy: false,
            mode: "direct",
            proxy_port: 0,
            codex_network_access: true,
            codex_status_section_default_visible: true,
        };
        assert!(
            one_million_catalog_ready(&paths, &direct_target),
            "direct(requires_proxy=false)应直接判 1M 就绪,不依赖 catalog 文件"
        );
    }
}
