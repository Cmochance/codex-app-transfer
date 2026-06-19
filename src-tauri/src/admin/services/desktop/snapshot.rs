use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use codex_app_transfer_codex_integration::{
    apply_provider, catalog_models_for_provider_with_display_names, ensure_file_store_mode,
    get_snapshot_status, has_snapshot, has_stale_active_snapshot, list_snapshots, read_auth,
    restore_available_count, restore_codex_snapshot, restore_codex_state, sync_mcp_credentials,
    ApplyConfig, CodexPaths,
};
use codex_app_transfer_gemini_oauth::antigravity_static_models;
use codex_app_transfer_proxy::proxy_telemetry;
use codex_app_transfer_registry::RawConfig;
use serde_json::{json, Value};

use crate::admin::handlers::common::{active_provider_name, read_setting_bool, APP_VERSION};
use crate::admin::handlers::providers::{
    active_provider, provider_default_model, provider_display_name, provider_index,
    provider_model_capabilities, provider_model_mappings, provider_review_model_slot,
    provider_supports_1m,
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
    /// [MOC-69] model id → 人类可读 displayName(JSON object)。仅 antigravity 非空
    /// (从 static seed 构建);Codex Desktop model catalog 的 `display_name` 优先用它,
    /// 让 Codex 自己的 model picker 显示 displayName 而非 raw id。其他 provider 为
    /// `Value::Null`,catalog 回退 raw id(行为不变)。
    pub model_display_names: Value,
    /// [MOC-173] auto-review 审查模型槽位 key(如 `gpt_5_4`),`None` = auto-review 复用主模型。
    /// 从 provider `reviewModelSlot` 读;透传给 catalog 生成写每个 entry 的
    /// `auto_review_model_override`,让审查脱钩主模型走该槽位的现有映射。
    pub review_model_slot: Option<String>,
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

    // [MOC-234] **所有 provider 统一走 local_proxy**(含 `apiFormat=responses` /
    // `openai_responses`)。此前 responses 协议在填了 baseUrl + apiKey 时会 bypass
    // 代理、让 Codex.app 直连上游(direct 模式),导致这条流量不进转发层 —— 无法做
    // 协议层处理 / 埋点 / 上下文与用量归因。MOC-234 永久去掉 direct:responses↔responses
    // 改由代理内 `ResponsesPassthroughAdapter` 做 1:1 字节直透(compact / web_search /
    // namespace 等 Codex 自有 / 上游原生能力照样原样转发、本项目不接管,体验不降级),
    // 既把原生 Responses 上游纳入统一管线,又能在一处做只读整合。
    //
    // local_proxy 模式:Codex.app → 127.0.0.1:18080 → 本地代理(协议转换 / 1:1 透传 +
    // extras 注入 + model 改写 + vision 剥离 + namespace MCP 展平等)→ 上游。
    let api_format_lower = provider
        .get("apiFormat")
        .and_then(|v| v.as_str())
        .unwrap_or("openai_chat")
        .trim()
        .to_ascii_lowercase();

    let codex_network_access = crate::admin::handlers::proxy::read_codex_network_access(cfg);

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
        model_display_names: antigravity_display_names(&api_format_lower),
        review_model_slot: provider_review_model_slot(provider),
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

/// [MOC-178 codex P2] 当前 active provider 是否支持真实账号 relay = 有 active provider 且走 proxy
/// (requires_proxy=true)。**无 provider**(默认 activeProvider null、无 target)→ false:没法
/// apply relay,保不住 chatgpt 态,不该开真实账号模式 flag(startup 收敛关 / pin 只 save 镜像)。
/// [MOC-234] direct 直连已移除,所有 provider 恒 requires_proxy=true,故等价于「有无 active provider」。
/// 只读,不写盘。
pub fn active_provider_supports_relay() -> bool {
    crate::admin::registry_io::load()
        .ok()
        .and_then(|cfg| desktop_target_for_active_provider(&cfg).map(|t| t.requires_proxy))
        .unwrap_or(false)
}

pub fn desktop_expected_model_items(target: &DesktopConfigTarget) -> Vec<Value> {
    catalog_models_for_provider_with_display_names(
        &target.provider_name,
        &target.default_model,
        target.supports_1m,
        Some(&target.model_mappings),
        Some(&target.model_capabilities),
        Some(&target.model_display_names),
        target.review_model_slot.as_deref(),
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
    // 防御性 guard:不经代理的 target(`requires_proxy=false`)不写 model_catalog_json,
    // 「1M catalog 是否写入」对它无意义 → 视为就绪,否则 desktop_health 会因 config.toml
    // 没有 model_catalog_json 而永远返回 false → needsApply 死循环。
    // [MOC-234] direct 直连已移除后,desktop target 恒 `requires_proxy=true`,此分支当前
    // 不会被命中,仅作防御保留(未来若再出现不经代理的 target 仍正确)。
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
    apply_desktop_target_impl(target, false)
}

/// [MOC-178] 清除真实账号专用:强制 non-relay(不看 active_is_real_chatgpt_now),apply 写
/// auth_mode=apikey + OPENAI_API_KEY 但**保留 tokens**(MANAGED 只 auth_mode/OPENAI_API_KEY),
/// 使 toggle 关 + Codex 原生不显示 plugins,而退出 restore 仍能写回 chatgpt + tokens 完整恢复
/// (对比"删活动 auth.json"会丢 tokens、restore 恢复不回)。
pub fn apply_desktop_target_clearing_real(target: &DesktopConfigTarget) -> Result<Value, String> {
    apply_desktop_target_impl(target, true)
}

fn apply_desktop_target_impl(
    target: &DesktopConfigTarget,
    force_apikey: bool,
) -> Result<Value, String> {
    let paths = CodexPaths::from_home_env().map_err(|e| e.to_string())?;
    // [MOC-104] relay 模式 gate:活动已是可用真实 chatgpt 时,apply 保留 chatgpt
    // 登录态(让 Codex 原生显示 Plugins 入口、不再依赖 CDP daemon 注入,消除 MOC-100
    // 高延迟)。[MOC-178] force_apikey(清除真实账号)强制不 relay,即便活动仍是 chatgpt。
    // [MOC-234] direct 直连已永久移除,所有 provider 走 local_proxy,故不再有「direct 不 relay」分支。
    let preserve_chatgpt_auth =
        !force_apikey && crate::codex_real_account::active_is_real_chatgpt_now();
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
            review_model_slot: target.review_model_slot.as_deref(),
            app_version: APP_VERSION,
            codex_network_access: target.codex_network_access,
            preserve_chatgpt_auth,
        },
    )
    .map_err(|e| format!("apply 失败: {e}"))?;
    serde_json::to_value(result).map_err(|e| format!("apply 结果序列化失败: {e}"))
}

pub async fn sync_desktop_for_active_provider(state: &AdminState) -> Value {
    let result = sync_desktop_for_active_provider_impl(state, false).await;
    // [MOC-257 review] OFF 不变量维护:apply_provider 会重建一份 apikey auth.json,撤销 OFF 的「无
    // auth.json」。任何 sync 路径(重启 Codex / 切 provider)apply 之后,若**持久模式仍是 off** 则
    // 重新清掉(真账号先 stash 兜底再清),否则用户选 OFF 后一重启 Codex / 切 provider 就丢了「无
    // auth.json」态直到再 toggle。synthetic/real 模式(非 off)不触发。
    if crate::codex_real_account::resolve_plugin_unlock_mode()
        == crate::codex_real_account::PluginUnlockMode::Off
    {
        // [MOC-257 review] 失败留痕(只读 ~/.codex / 文件锁 / 磁盘满):OFF 的「无 auth.json」不变量被
        // sync 重建的 apikey auth.json 悄悄破坏却无声 → 至少 error 日志可诊断(真账号已 stash 不丢)。
        if let Err(e) = crate::codex_real_account::stash_displaced_real_auth().await {
            tracing::error!("[PluginUnlock] OFF sync 后 stash 失败: {e}");
        }
        if let Err(e) = crate::codex_real_account::clear_active_auth_file().await {
            tracing::error!(
                "[PluginUnlock] OFF sync 后重新清 auth.json 失败(无 auth.json 不变量未维持): {e}"
            );
        }
        codex_app_transfer_proxy::set_fake_account_mode(false);
    }
    result
}

/// [MOC-178] 清除真实账号:apply 当前 active provider 强制切 apikey(写 auth_mode=apikey、
/// 保留 tokens),停用真实账号但退出 restore 能完整恢复 chatgpt。见 forget_handler。
pub async fn sync_desktop_clearing_real_account(state: &AdminState) -> Value {
    sync_desktop_for_active_provider_impl(state, true).await
}

/// [MOC-257 三态] 应用插件解锁三态:设活动 auth.json + 驱动 proxy 伪造 atomic + apply(relay/非relay)。
///
/// relay 写 chatgpt_base_url 仍复用现有 gate(`apply_desktop_target_impl` 的 `active_is_real_chatgpt_now`)——
/// synthetic/real 两态都先把活动 auth.json 写成 `auth_mode=chatgpt`(activate_fake/real),故 gate 自然
/// 为真 → 写 chatgpt_base_url(等价「非 off 一律写」);off 让活动腾空/apikey → gate 为假 → 不写。
///
/// **真账号 stash 时序**:synthetic/off 切走真账号前先 `stash_displaced_real_auth`(整文件保 tokens);
/// real 切回先 `restore_stashed_real_auth`。退出/启动 self-heal 的还原由 main.rs 在「现有 restore 之前」
/// 调 `restore_stashed_real_auth_blocking`,把 managed key 补到真 auth.json(而非缺文件得到的空壳),
/// 不与现有 restore 冲突。任一步失败:真账号已 stash 仍安全(stash + 退出/启动 restore 兜底)。
pub async fn apply_plugin_unlock_mode(
    state: &AdminState,
    mode: crate::codex_real_account::PluginUnlockMode,
) -> Result<(), String> {
    use crate::codex_real_account as ra;
    use crate::codex_real_account::PluginUnlockMode as M;
    // [MOC-257 P1 review] **改 ~/.codex 之前**先捕获原始快照(若还没有)。**三态都要**:synthetic/real 会
    // 写合成/真 auth(否则首次 apply 时合成 auth 先写入、apply_provider 才取快照 → 快照把合成当「用户原始
    // 态」,退出 restore 还原成合成 → standalone Codex 拿假凭据撞 chatgpt.com);**OFF 会 clear auth.json**
    // ——用户原有 apikey(非真账号、stash 不收)无既有快照时直接被删、restore 无快照可还原 → 原始
    // OPENAI_API_KEY 永久丢失(P1)。`snapshot_codex_state` 幂等(`has_snapshot` 已有则不覆盖)。
    // [MOC-257 review] **同时查 stale 快照**:restoreCodexOnExit=false 保留态下只剩 stale 快照(= 真·原始
    // baseline),has_snapshot=false;若只查它,这里会对**当前已被 Transfer 改过的状态**(合成 auth + relay)
    // 拍新快照、把 stale 原始归档掉 → 后续 restore 用被投毒的当前快照还原成合成。对齐 desktop_clear 的双查。
    if let Ok(paths) = CodexPaths::from_home_env() {
        if !has_snapshot(&paths) && !has_stale_active_snapshot(&paths) {
            let cfg = load_registry().ok();
            let pname = cfg.as_ref().map(active_provider_name).unwrap_or_default();
            // [MOC-257 review] 传当前 proxyPort 给 signature-strip(不止 18080),自定义端口的 relay 字段也反投毒。
            let proxy_port = cfg.as_ref().map(read_proxy_port).unwrap_or(18080);
            if let Err(e) = codex_app_transfer_codex_integration::snapshot_codex_state(
                &paths,
                APP_VERSION,
                &pname,
                &[proxy_port, 18080],
            ) {
                // [MOC-257 P1 review] 无既有快照 + 捕获失败(快照目录不可写而 ~/.codex 仍可写)→ **abort 不
                // mutate**:否则下面 OFF clear / synthetic 覆写会改 ~/.codex 却无快照可还原(原 apikey /
                // OPENAI_API_KEY 永久丢失)。宁可这次切换失败,也不毁原配置。
                return Err(format!(
                    "无法捕获 Codex 原配置快照,已中止切换以免丢失原配置(请检查 ~/.codex-app-transfer 目录权限): {e}"
                ));
            }
        }
    }
    let result = match mode {
        M::Off => {
            // 真账号 stash 走、合成残留清 → 确保 ~/.codex 无 auth.json(用户硬性要求);proxy 不伪造。
            // **不**跑 sync_clearing —— 它(force_apikey apply)会重写一份 apikey auth.json,违背「无
            // auth.json」。config.toml 残留的 relay 字段(chatgpt_base_url)在无 auth.json / 无 chatgpt
            // 账号时 inert(Codex 不会发 /backend-api),退出时 restore_codex_if_enabled 会清。`state`
            // 在 OFF 分支不需要(不 apply provider),由 synthetic/real 分支使用。
            let _ = state;
            ra::stash_displaced_real_auth().await?;
            if let Err(e) = ra::clear_active_auth_file().await {
                // 真账号已安全 stash;仅残留合成/apikey 没删干净。留痕(真账号不丢,退出 restore 兜底)。
                tracing::error!(
                    "[PluginUnlock] OFF 清空活动 auth.json 失败(真账号已 stash 安全): {e}"
                );
                return Err(e);
            }
            codex_app_transfer_proxy::set_fake_account_mode(false);
            Ok(())
        }
        M::Synthetic => {
            // 活动若是真账号先 stash 保全(也避开 activate 的真账号守护);写合成 → 活动=chatgpt;
            // proxy 伪造开;apply relay。**事务化**:activate / relay 任一失败都回滚(还原 stash 真账号回活动
            // 覆盖合成 / 无真账号则清掉合成)+ 关伪造,让 caller(set/add/tray/startup)无需各自清半生效态。
            // [MOC-257 review] `displaced` = **本次是否真把真账号移进了 stash**(stash_displaced 返回)。回滚
            // 只在 displaced 时 un-stash 还原真账号回活动 —— 比旧 `!was_synthetic` 精确:已是合成态 / apikey /
            // 「已有可用 stash 仅归档过期残留」都 displaced=false,这些情形回滚 un-stash 会把本次没动的真账号
            // 误挪回活动(留 persisted synthetic + real-active 无 relay)。
            let displaced = ra::stash_displaced_real_auth().await?;
            // 事务字节快照(stash 后的活动:真账号已移走、剩 apikey / 空 / 旧合成)。activate 覆写成合成,
            // 失败回滚:还原它(恢复原 apikey)。否则从 apikey 切 synthetic 失败时下面只清合成 → 原 apikey 丢
            // (stash 不收 apikey)。
            let pre_synth = ra::snapshot_active_auth_bytes();
            if let Err(e) = ra::activate_fake_account().await {
                tracing::error!("[PluginUnlock] synthetic activate 失败,回滚: {e}");
                ra::restore_active_auth_bytes(pre_synth);
                if displaced {
                    // [MOC-257 review] 失败别静默吞 + **surface 给 caller**:真账号本次已 displace 到 stash,还原
                    // 它回活动若失败(内部可能已删活动文件才在 rename 失败 → active 缺失),把清理失败连同原错
                    // 一起返回,让 UI 提示「切换失败且清理未完成,重启可自动恢复」,而非只报原错、静默留 active
                    // 缺失。真账号本身未丢(仍在 stash,下次启动 self-heal 重试)。
                    if let Err(e2) = ra::restore_stashed_real_auth().await {
                        tracing::error!(
                            "[PluginUnlock] synthetic 回滚还原 stash 真账号失败(真账号仍在 stash,待启动 self-heal): {e2}"
                        );
                        return Err(format!(
                            "切换失败({e});回滚还原真账号也失败({e2})—— 真账号仍安全在 stash,重启 Codex App Transfer 会自动恢复"
                        ));
                    }
                }
                return Err(e);
            }
            codex_app_transfer_proxy::set_fake_account_mode(true);
            if let Err(e) = ensure_relay_applied(state).await {
                // relay 没装上 → 别留「合成 active + 伪造开但无 relay」(Codex 直连 chatgpt.com 撞 401)。
                // 还原原活动(apikey);只有本次 displace 了真账号才还原它回活动,否则它本就在 stash 留着。
                codex_app_transfer_proxy::set_fake_account_mode(false);
                ra::restore_active_auth_bytes(pre_synth);
                if displaced {
                    // [MOC-257 review] 失败别静默吞 + **surface 给 caller**:真账号本次已 displace 到 stash,还原
                    // 它回活动若失败(内部可能已删活动文件才在 rename 失败 → active 缺失),把清理失败连同原错
                    // 一起返回,让 UI 提示「切换失败且清理未完成,重启可自动恢复」,而非只报原错、静默留 active
                    // 缺失。真账号本身未丢(仍在 stash,下次启动 self-heal 重试)。
                    if let Err(e2) = ra::restore_stashed_real_auth().await {
                        tracing::error!(
                            "[PluginUnlock] synthetic 回滚还原 stash 真账号失败(真账号仍在 stash,待启动 self-heal): {e2}"
                        );
                        return Err(format!(
                            "切换失败({e});回滚还原真账号也失败({e2})—— 真账号仍安全在 stash,重启 Codex App Transfer 会自动恢复"
                        ));
                    }
                }
                // [MOC-257 review] 上面 set_fake(false) 是回滚期防护;**还原后**按活动是否合成重置伪造:在已是
                // 合成态上 re-apply synthetic、relay 失败还原回合成(displaced=false)→ 重开伪造,否则合成 token
                // 经现存 relay 透传 chatgpt.com 撞 401;displaced 还原成真账号则保持关。
                codex_app_transfer_proxy::set_fake_account_mode(ra::active_is_synthetic());
                return Err(e);
            }
            Ok(())
        }
        M::Real => {
            // [MOC-257 review] 事务化:在 restore_stashed **之前**快照 pre-apply 活动(可能是合成 / off 空)——
            // 失败回滚要还原它,而非 restore_stashed **之后**已 unstash 的真账号(还原成后者会留 real-active 无
            // relay)。activate 还会把 apikey-with-tokens 改写成 chatgpt + 移除 `OPENAI_API_KEY`,字节快照保
            // tokens + gateway key(deactivate 只翻 auth_mode 会丢 key → apikey 模式无 key、对话挂)。
            let pre_apply = ra::snapshot_active_auth_bytes();
            // 记是否真从 stash 还原了 + 还原出的真账号字节;失败回滚时 re-stash 回去(恢复「真账号在 stash」,
            // 否则还原活动后真账号丢)。restore_stashed:active 非可用真账号才覆盖。[MOC-257 review] 它内部可能
            // 已删活动 auth 才在 rename stash 时失败 → **不能直接 `?` 退出**(会留活动被删、pre_apply 未还原);
            // 同 activate/relay 失败,先还原 pre_apply 再返。
            let restored_from_stash = match ra::restore_stashed_real_auth().await {
                Ok(v) => v,
                Err(e) => {
                    ra::restore_active_auth_bytes(pre_apply);
                    return Err(e);
                }
            };
            let unstashed_real = if restored_from_stash {
                ra::snapshot_active_auth_bytes()
            } else {
                None
            };
            // activate 返 false = 没能弄出可用真账号 → 报错(绝不带合成 active 关伪造去 relay,会把合成 token
            // 发真 chatgpt.com 全 401);effective-degrade 已挡 real-but-unusable,这是 TOCTOU 防御。
            if !ra::activate_real_account().await.unwrap_or(false) {
                // [MOC-257 review] 先 re-stash 真账号(fallible),成功才 restore pre_apply 覆盖活动;失败 → 把真
                // 账号写回**活动**(内存副本保证不丢)+ surface,绝不静默丢真 tokens(原 stash 已被 unstash 消费、
                // 内存 Vec 是唯一副本)。
                if let Err(e2) = ra::restash_real_auth_bytes(unstashed_real.clone()) {
                    ra::restore_active_auth_bytes(unstashed_real);
                    return Err(format!(
                        "无法激活真实账号(账号不可用 / 已失效);回滚 re-stash 真账号也失败({e2})—— 真账号已保留在活动 auth、未丢失,请在 Codex 重新登录后重试"
                    ));
                }
                ra::restore_active_auth_bytes(pre_apply);
                return Err(
                    "无法激活真实账号(账号不可用 / 已失效),请在 Codex 重新登录后再切「真实账号」"
                        .to_owned(),
                );
            }
            codex_app_transfer_proxy::set_fake_account_mode(false);
            // relay 没装上 → 还原 pre-apply 活动 + re-stash 真账号,别留「real chatgpt active 无 relay」(Codex
            // 据 auth_mode=chatgpt 直连真 chatgpt.com 绕过 proxy)。下次启动 resolve→real 再激活。
            if let Err(e) = ensure_relay_applied(state).await {
                // 先 re-stash(fallible),成功才 restore pre_apply + 重置伪造;失败 → 真账号写回活动保不丢 + surface。
                if let Err(e2) = ra::restash_real_auth_bytes(unstashed_real.clone()) {
                    ra::restore_active_auth_bytes(unstashed_real);
                    return Err(format!(
                        "切换失败({e});回滚 re-stash 真账号也失败({e2})—— 真账号已保留在活动 auth、未丢失,重启 Codex App Transfer 会自动恢复"
                    ));
                }
                ra::restore_active_auth_bytes(pre_apply);
                // [MOC-257 review] 上面已 set_fake(false)。若 pre-apply 是合成态(prior synthetic),还原后要把
                // 伪造**重新开上**(按还原后的活动是否合成决定)——否则合成 token 经现存 relay 透传 chatgpt.com
                // 撞 401;非合成态(apikey/off)保持关。
                codex_app_transfer_proxy::set_fake_account_mode(ra::active_is_synthetic());
                return Err(e);
            }
            Ok(())
        }
    };
    // [MOC-257 review] 成功生效 → 记录最近 apply 的模式,供 status 如实报告「当前实际生效」(外部
    // codex login 让 resolve 升级但未 apply 时,报 resolve 会显示 Real 却仍 fabricate /backend-api)。
    if result.is_ok() {
        ra::record_applied_mode(mode);
    }
    result
}

/// apply active provider 并校验 relay 真生效(active 仍 chatgpt + sync success)。供 synthetic/real 共用。
async fn ensure_relay_applied(state: &AdminState) -> Result<(), String> {
    let synced = sync_desktop_for_active_provider(state).await;
    let ok = synced
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if ok && crate::codex_real_account::active_is_real_chatgpt_now() {
        Ok(())
    } else {
        Err("apply relay 未生效(系统代理未起 / 无可用 provider?),请检查后重试".to_owned())
    }
}

async fn sync_desktop_for_active_provider_impl(state: &AdminState, force_apikey: bool) -> Value {
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
                // [MOC-178 codex P2] 清除真实账号(force_apikey)只需切 auth_mode=apikey、不依赖
                // proxy。proxy 起不来(端口冲突等)时,正常 apply 仍 return 失败;但 force_apikey
                // 必须继续走到 auth rewrite —— 否则 flag 已置 false、镜像已删,活动却留 chatgpt,
                // UI 显示 off 但 Codex 还显示 plugins(状态不一致)。故 force_apikey 下只 warn、继续。
                if !force_apikey {
                    return json!({"attempted": true, "success": false, "mode": target.mode, "requiresProxy": target.requires_proxy, "message": e});
                }
                tracing::warn!(
                    "[MOC-178] 清除真实账号:proxy 起不来({e}),仍继续切 apikey(auth rewrite 优先)"
                );
            }
        }
    } else {
        state.proxy_manager.stop_silent();
    }

    let apply_result = if force_apikey {
        apply_desktop_target_clearing_real(&target)
    } else {
        apply_desktop_target(&target)
    };
    match apply_result {
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

/// [MOC-257 review] 从 `http://127.0.0.1:<port>` / `http://localhost:<port>`(Codex config.toml 的
/// `chatgpt_base_url`/`openai_base_url`)解析出端口,供保留 relay 的 proxy 在 **Codex 实际指向的端口**重启。
fn parse_local_proxy_port(url: &str) -> Option<u16> {
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| url.strip_prefix("http://localhost:"))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

pub async fn auto_apply_on_startup_if_enabled(proxy_manager: Arc<ProxyManager>) -> Value {
    let cfg = match load_registry() {
        Ok(cfg) => cfg,
        Err(e) => {
            return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": format!("failed: {e}")})
        }
    };
    if !read_setting_bool(&cfg, "autoApplyOnStart", true) {
        // [MOC-257 review] autoApplyOnStart=false 跳过 apply,但若盘上是**保留的 relay 态**(restoreCodexOnExit=
        // false 把 auth + config.toml 的 chatgpt_base_url/openai_base_url→本地 proxy 留下、上次退出停了 proxy),
        // 仍要把 proxy 起起来 —— 否则 Codex 据这些 base_url 把 chat + /backend-api 发到死端口、全挂。**synthetic
        // 与 real relay 都算**(real:真账号 + relay 透传;synthetic:合成 + 伪造):统一看 config.toml 是否指向
        // 本地 proxy(active_is_synthetic 兜底,防极端只写 auth 没写 relay)。早期预置只开伪造 flag,这里补进程。
        // [MOC-257 review] 只在 **Transfer-owned** relay 时接管端口:合成 auth(Transfer 写的)或 Transfer 改过
        // ~/.codex(apply 前必拍快照 has_snapshot)。否则 config.toml 上的 localhost base_url 可能是用户**自己**
        // 的本地 provider(Ollama / LM Studio `http://localhost:11434/v1`)—— 别去 hijack 它的端口(会拦 Codex
        // 流量 / 占住真本地服务端口)。端口仍从 URL 解析(对齐上条:用户改过 proxyPort / 遗留端口时不能用
        // settings.proxyPort)。
        let paths = CodexPaths::from_home_env().ok();
        let synthetic = crate::codex_real_account::active_is_synthetic();
        // [MOC-257 review] **含 stale 快照**:restoreCodexOnExit=false 保留 real relay 时,证明 Transfer 拥有
        // 该 URL 的 active 快照属于**上个 session**(本 session 还没拍)→ `has_snapshot` 为 false 但
        // `has_stale_active_snapshot` 为 true。漏了它会让 real 保留态 transfer_applied=false、不恢复 proxy。
        let transfer_applied = paths
            .as_ref()
            .is_some_and(|p| has_snapshot(p) || has_stale_active_snapshot(p));
        // [MOC-257 review] **只认 chatgpt_base_url**,不认 openai_base_url:has_snapshot 可能是**旧** Transfer
        // apply 残留,而用户后来手动把 Codex 改成本地 provider(Ollama / LM Studio `openai_base_url =
        // "http://localhost:11434/v1"`)→ 认 openai_base_url 会绑 11434 抢用户的 Ollama 端口。chatgpt_base_url 是
        // ChatGPT 专用 relay key,本地 provider 永不设它 → 指向 localhost 必是 Transfer 的插件解锁 relay。
        let relay_port = if synthetic || transfer_applied {
            paths.as_ref().and_then(|p| {
                read_codex_toml_root_string(p, "chatgpt_base_url")
                    .and_then(|u| parse_local_proxy_port(&u))
            })
        } else {
            None
        };
        if relay_port.is_some() || synthetic {
            // Codex 实际指向的端口起 proxy;无 relay URL(纯 synthetic 兜底)回退 settings.proxyPort。
            let port = relay_port.unwrap_or_else(|| read_proxy_port(&cfg));
            let started = start_proxy_if_needed(&proxy_manager, port)
                .await
                .unwrap_or(false);
            return json!({"applied": false, "requiresProxy": true, "proxyStarted": started, "message": "auto-apply disabled; proxy started for preserved relay"});
        }
        return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": "disabled by settings"});
    }
    if active_provider(&cfg).is_none() {
        return json!({"applied": false, "requiresProxy": false, "proxyStarted": false, "message": "no active provider; skip"});
    }
    // trace_viewer 不参与 desktop sync;给一个未启动的空 manager 满足 AdminState 即可。
    let state = AdminState {
        proxy_manager,
        trace_viewer_manager: Arc::new(crate::trace_viewer::TraceViewerManager::new()),
    };
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
    // [MOC-197] stale session 快照(被强杀 session 遗留)也算"有快照"——
    // restore_codex_state 内部会兜底还原它(startup 自愈核心路径);只有
    // active/ 真空才 skip。
    if !has_snapshot(&paths) && !has_stale_active_snapshot(&paths) {
        return json!({"attempted": false, "restored": false, "success": true, "reason": reason, "message": "no snapshot; skip"});
    }
    match restore_codex_state(&paths) {
        Ok(restored) => {
            // [MOC-197 silent-failure HIGH#2] startup/exit 自愈是核心交付,caller
            // (main.rs)`let _ =` 丢弃返回值 → 这里必须自己留 audit trail,否则
            // 自愈失败的症状(残留 sandbox_mode → Codex 报"无法设置管理员沙盒")
            // 与未触发无法区分。
            tracing::info!("codex restore ({reason}): restored={restored}");
            json!({"attempted": true, "restored": restored, "success": true, "reason": reason})
        }
        Err(e) => {
            tracing::error!("codex restore ({reason}) failed: {e}");
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
    // trace_viewer 不参与 desktop sync;给一个未启动的空 manager 满足 AdminState 即可。
    let state = AdminState {
        proxy_manager,
        trace_viewer_manager: Arc::new(crate::trace_viewer::TraceViewerManager::new()),
    };
    let desktop_sync = sync_desktop_for_active_provider(&state).await;
    // MOC-62:切换后做一次 MCP 凭据镜像同步(镜像跟随 live 捕获新授权 + 传播登出删除,
    // 绝不写 live;只动两个凭据文件,不碰 config.toml)。
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

/// MOC-62:provider 交互式切换后做一次 MCP 凭据镜像同步(仅开关开时)。调
/// `sync_mcp_credentials` 让镜像跟随 live(捕获新授权 + 传播登出删除),**绝不写 live**
/// (live 缺失时只报告可恢复条数、由用户确认式 restore 处理),只动两个凭据文件、
/// **不碰 config.toml**。缩短"新授权还没进镜像就被外部擦掉"的窗口。
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
    use crate::admin::registry_io::save_for_test as save_registry;
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

        // [MOC-234] apiFormat=responses 的自定义第三方 provider 不再 direct 直连,
        // 跟其它 provider 一样走 local_proxy(代理内 1:1 字节透传),便于统一处理与埋点。
        let mut resp_cfg = config_with_secret();
        let resp_provider = json!({
            "id": "custom-third-party-instance",
            "name": "Custom Third-Party (Responses)",
            "baseUrl": "https://direct.example.com/v1/",
            "authScheme": "bearer",
            "apiFormat": "responses",
            "apiKey": "sk-direct",
            "models": {"default": "direct-model"},
        });
        let target = desktop_config_target_for_provider(&mut resp_cfg, &resp_provider, Some(19090));
        assert_eq!(
            target.mode, "local_proxy",
            "apiFormat=responses 也走 local_proxy(MOC-234 已去除 direct 直连)"
        );
        assert!(target.requires_proxy, "responses 现在也需要本地代理");
        assert_eq!(
            target.base_url, "http://127.0.0.1:19090",
            "config.toml 指向本地代理而非上游 baseUrl"
        );
        assert!(
            target.api_key.starts_with("cas_"),
            "auth.json 写 gateway key,上游 apiKey 由代理按 provider 注入"
        );
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
    fn openai_responses_alias_uses_local_proxy_no_direct_bypass() {
        // [MOC-234] openai_responses 别名跟 responses 一样走 local_proxy(代理内 1:1
        // 透传),不再 direct 直连上游 —— 流量进转发层才能做统一处理与埋点。
        let mut cfg = config_with_secret();
        let provider = json!({
            "id": "alias-responses",
            "name": "Alias Responses",
            "baseUrl": "https://api.openai.com/v1/",
            "authScheme": "bearer",
            "apiFormat": "openai_responses",
            "apiKey": "sk-direct",
            "models": {"default": "gpt-5"},
        });
        let target = desktop_config_target_for_provider(&mut cfg, &provider, Some(19090));
        assert_eq!(
            target.mode, "local_proxy",
            "openai_responses 别名走 local_proxy(MOC-234 已去除 direct bypass)"
        );
        assert!(target.requires_proxy);
        assert_eq!(target.base_url, "http://127.0.0.1:19090");
    }

    #[test]
    fn switch_between_providers_keeps_proxy_and_repoints_config() {
        // [MOC-234] responses provider 现在也走 local_proxy(不再 direct 直连),
        // 故在 responses ↔ builtin 之间切换:代理始终运行,config.toml 始终指向
        // 127.0.0.1,且绝不把上游 apiKey(sk-direct)写进 auth.json(由代理注入)。
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
                        "name": "Custom Responses",
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
                assert_eq!(r1["desktopSync"]["mode"], json!("local_proxy"));
                assert!(
                    manager.status().running,
                    "responses 也走代理,proxy 必须运行"
                );
                let toml1 = fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
                assert!(
                    toml1.contains("openai_base_url = \"http://127.0.0.1:"),
                    "responses 经代理,config.toml 指向 127.0.0.1 而非上游:\n{toml1}"
                );
                assert!(
                    !toml1.contains("direct.example.com"),
                    "禁止把上游 URL 写进 config.toml:\n{toml1}"
                );

                let p1_id = cfg["providers"][0]["id"].as_str().unwrap().to_owned();
                let r2 = switch_provider_and_sync(Arc::clone(&manager), p1_id).await;
                assert_eq!(r2["desktopSync"]["mode"], json!("local_proxy"));
                assert!(manager.status().running, "切回 builtin 代理仍运行");
                let toml2 = fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
                assert!(
                    toml2.contains("openai_base_url = \"http://127.0.0.1:"),
                    "config.toml 必须指向 127.0.0.1,实际:\n{toml2}"
                );
                assert!(
                    !toml2.contains("direct.example.com"),
                    "禁止残留上游 URL:\n{toml2}"
                );
                let auth_json: Value = serde_json::from_str(
                    &fs::read_to_string(home.join(".codex").join("auth.json")).unwrap(),
                )
                .unwrap();
                let api_key = auth_json["OPENAI_API_KEY"].as_str().unwrap_or_default();
                assert_ne!(
                    api_key, "sk-direct",
                    "auth.json 不能残留上游 provider apiKey(由代理注入)"
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
                // [MOC-154] codexNetworkAccess 新默认 false;此测试验证 full-access apply 场景,显式开
                cfg["settings"]["codexNetworkAccess"] = json!(true);
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
    fn provider_switch_syncs_responses_via_local_proxy() {
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
                        "name": "Custom Responses",
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

                // [MOC-234] responses 走 local_proxy:代理启动、config.toml 指向 127.0.0.1、
                // 上游 apiKey 不写进 auth.json(由代理按 provider 注入)。
                assert_eq!(result["success"], json!(true));
                assert_eq!(result["desktopSync"]["success"], json!(true));
                assert_eq!(result["desktopSync"]["mode"], json!("local_proxy"));
                assert_eq!(result["desktopSync"]["requiresProxy"], json!(true));
                assert!(
                    manager.status().running,
                    "responses 走 local_proxy,代理必须启动"
                );

                let saved = load_registry().unwrap();
                assert_eq!(saved["activeProvider"], json!("p2"));
                let config_toml =
                    fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
                assert!(
                    config_toml.contains("openai_base_url = \"http://127.0.0.1:"),
                    "config.toml 必须指向本地代理 127.0.0.1,实际:\n{config_toml}"
                );
                assert!(
                    !config_toml.contains("direct.example.com"),
                    "禁止把上游 URL 写进 config.toml:\n{config_toml}"
                );
                let auth_json: Value = serde_json::from_str(
                    &fs::read_to_string(home.join(".codex").join("auth.json")).unwrap(),
                )
                .unwrap();
                let api_key = auth_json["OPENAI_API_KEY"].as_str().unwrap_or_default();
                assert_ne!(
                    api_key, "sk-direct",
                    "auth.json 不写上游 provider apiKey(由代理注入)"
                );
                assert!(!api_key.is_empty(), "auth.json 必须有 gateway key");

                manager.stop_silent();
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
        let raw = serde_json::to_string(&desktop_expected_model_items(&target)).unwrap();

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
        // [MOC-154] gpt_5_4_mini 槽未配置 → 跳过;fallback entry(slug=实际模型名)已删
        assert!(
            !names.contains(&"gpt-5.4-mini"),
            "empty slot should be skipped"
        );
        assert!(
            !names.contains(&"deepseek-v4-pro"),
            "fallback entry removed in MOC-154"
        );
        // [MOC-154] kimi-k2(258_400) / glm-4.6(200_000) 均非 1M;旧逻辑 fallback entry
        // deepseek-v4-pro[1m] 已删,无 supports1m=true 的 entry
        assert!(
            models
                .iter()
                .all(|item| item.get("supports1m").and_then(|v| v.as_bool()) != Some(true)),
            "kimi-k2 / glm-4.6 均非 1M,不应有 supports1m=true"
        );
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

    /// 防御性 guard 回归保护:不经代理的 target(`requires_proxy=false`)不写
    /// model_catalog_json,`one_million_catalog_ready` 必须直接判就绪(short-circuit),
    /// 不读 catalog 文件。否则带 [1m] 的 provider 会让 desktop_health 永远 needsApply=true。
    /// 短路在读文件之前,即便 paths 指向不存在目录也成立。[MOC-234] direct 移除后此态为
    /// 合成态(prod 恒 requires_proxy=true),仍单测 guard 本身正确。
    #[test]
    fn one_million_catalog_ready_short_circuits_when_proxy_not_required() {
        let paths = CodexPaths::from_home_dir(std::path::Path::new("/nonexistent-cas-317"));
        let no_proxy_target = DesktopConfigTarget {
            base_url: "https://up.example.com/v1".into(),
            api_key: "sk".into(),
            supports_1m: true,
            provider_name: "Custom".into(),
            default_model: "gpt-5.5[1m]".into(),
            model_mappings: Value::Null,
            model_capabilities: Value::Null,
            model_display_names: serde_json::Value::Null,
            review_model_slot: None,
            requires_proxy: false,
            mode: "local_proxy",
            proxy_port: 0,
            codex_network_access: true,
        };
        assert!(
            one_million_catalog_ready(&paths, &no_proxy_target),
            "requires_proxy=false 应直接判 1M 就绪,不依赖 catalog 文件"
        );
    }
}
