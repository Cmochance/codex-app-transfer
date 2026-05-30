//! Apply / restore 主入口.

use serde::{Deserialize, Serialize};

use crate::auth::{read_auth, write_auth};
use crate::model_catalog::{
    catalog_models_for_provider, clear_catalog_models, upsert_catalog_models,
    CODEX_MODEL_CATALOG_KEY,
};
use crate::paths::CodexPaths;
use crate::snapshot::{
    drop_all_snapshots, drop_snapshot, drop_snapshot_by_id, has_snapshot, list_snapshots,
    read_snapshot_auth, read_snapshot_auth_by_id, read_snapshot_config, read_snapshot_config_by_id,
    snapshot_codex_state, snapshot_table_field_literal, snapshot_toml_value_literal,
};
use crate::toml_sync::{sync_root_value, sync_table_field, toml_string_literal};
use crate::CodexError;

/// 我们 apply 时实际触碰的 auth 字段(restore 时只动这些,其它字段保留)。
const MANAGED_AUTH_KEYS: &[&str] = &["auth_mode", "OPENAI_API_KEY"];

/// 我们 apply 时实际触碰的 config.toml 根级别字段(restore 时只动这些)。
const MANAGED_TOML_KEYS: &[&str] = &[
    "openai_base_url",
    "model_context_window",
    CODEX_MODEL_CATALOG_KEY,
    "model",
    "model_provider",
    // #212 / #215:`sandbox_mode` + `approval_policy` 一对(Codex docs "Full
    // access" 配对:`danger-full-access` + `never`)。toggle on 写两条让
    // 模型完全无审批联网;off 时全 strip 让 Codex 回 default(read-only +
    // on-request)。仅写 sandbox_mode 不够 —— Codex 默认 approval_policy =
    // OnRequest(`protocol.rs::AskForApproval` `#[default] OnRequest`),
    // 即便 sandbox 允许,Codex `is_safe_command()` 不认的命令仍弹审批。
    "sandbox_mode",
    "approval_policy",
];

/// 我们 apply 时实际触碰的 `[section]` 段内字段。restore 时按 `(section, key)`
/// 逐个 strip,**保留** section header 跟其它用户 key,避免误删。
///
/// #212 起加 `sandbox_workspace_write.network_access`(用 TOML section-table
/// 形式写,跟 Codex docs / `codex exec` 输出对齐;**不可** 用 root-level dotted
/// key 形式,会跟用户已有 `[sandbox_workspace_write]` 段并存触发 duplicate
/// table parse error,详见 [`crate::toml_sync::sync_table_field`])。
const MANAGED_TOML_TABLE_FIELDS: &[(&str, &str)] = &[("sandbox_workspace_write", "network_access")];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyConfig<'a> {
    /// 代理 base URL,例如 `http://127.0.0.1:18080`。
    pub base_url: &'a str,
    /// gateway API key(`cas_...`),会写到 auth.json。空字符串表示移除。
    pub gateway_api_key: &'a str,
    /// 当前 active provider 默认模型是否支持 1M 上下文。
    /// 为 `true` 时 config.toml 会被注入 1M 兼容配置。
    pub supports_1m: bool,
    /// 当前 active provider 的展示名,用于生成 Codex model catalog。
    #[serde(default)]
    pub provider_name: &'a str,
    /// 当前 active provider 的默认真实模型 ID,用于生成 Codex model catalog。
    #[serde(default)]
    pub default_model: &'a str,
    /// 当前 active provider 的模型槽位映射,用于让 catalog 与 proxy 路由一致。
    #[serde(skip)]
    pub model_mappings: Option<&'a serde_json::Value>,
    /// 当前 active provider 的模型能力声明,用于按目标模型声明窗口。
    #[serde(skip)]
    pub model_capabilities: Option<&'a serde_json::Value>,
    /// 应用版本(写入快照 manifest,便于诊断)。
    pub app_version: &'a str,
    /// 是否允许 Codex shell 工具网络访问(写入 `[sandbox_workspace_write]
    /// network_access` section field)。控制小白用户能否用 `curl` 等命令联网。
    /// Caller 从 `Settings.codex_network_access`(默认 `true`)读取(#212)。
    pub codex_network_access: bool,
    /// Codex Desktop 对话页底部 context 圆环 + tokens/s 显示开关(#258)。
    /// 写到 `~/.codex/.codex-global-state.json` 的
    /// `electron-persisted-atom-state.local-conversation-status-section-visible`。
    /// `true` → ensure 开启,`false` → ensure 关闭。
    /// Caller 从 `Settings.codex_status_section_default_visible`(默认 `true`)读取。
    pub codex_status_section_default_visible: bool,
    /// **direct 直连模式**(`bypass_proxy`,snapshot.rs:87-89)。为 `true` 时
    /// apply 只写上游配置(`openai_base_url` + auth key),并 **strip** 所有
    /// transfer 私货字段(`model_catalog_json` / catalog models /
    /// `model_context_window` / `sandbox_mode` / `approval_policy`)—— 这一步
    /// 同时是「从 local_proxy 切到 direct 时清掉残留私货」的清理机制;
    /// status-section atom 既不写也不清(留用户原值)。详见 issue #317。
    #[serde(default)]
    pub direct: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplyResult {
    pub config_toml_path: String,
    pub auth_json_path: String,
    pub snapshot_taken: bool,
    pub model_context_window_set: bool,
    pub model_catalog_json_set: bool,
}

/// 把 active provider 配置写入 `~/.codex/{config.toml,auth.json}`,
/// 首次写入前自动 snapshot。
pub fn apply_provider(paths: &CodexPaths, cfg: &ApplyConfig) -> Result<ApplyResult, CodexError> {
    // 1. snapshot(幂等;已有快照不会覆盖)
    let snapshot_taken_now = !has_snapshot(paths);
    snapshot_codex_state(paths, cfg.app_version, cfg.provider_name)?;

    // 2. config.toml: openai_base_url
    if cfg.base_url.is_empty() {
        sync_root_value(&paths.config_toml, "openai_base_url", None)?;
    } else {
        let literal = toml_string_literal(cfg.base_url);
        sync_root_value(&paths.config_toml, "openai_base_url", Some(&literal))?;
    }

    // 2b. **无条件 strip model_provider 字段**(#258 验证后强约束):
    //
    // 用户硬性要求 — 使用本项目时 config.toml **必须没有 model_provider 这一项**。
    // 残留 user 之前配过的 model_provider(任何值,包括 "openai")可能让 Codex CLI
    // 走非预期 provider 路径,**导致回话丢失**(endpoint 走 stale custom block /
    // history 链断 / autocompact 找不到 prior turn 等)。
    //
    // 等价性:Codex CLI 在缺失字段时 fallback 到 openai,跟显式写
    // `model_provider = "openai"` 行为等价(都会读 openai_base_url 走 proxy);
    // 但**只有 strip 才能确保不残留任何 user-set 值**。
    //
    // 不留 env opt-in:任何允许写回 `model_provider = "openai"` 的 escape hatch
    // 都可能让上面"回话丢失"风险复发,所以无条件 strip。如果未来 Codex CLI
    // 真的需要显式 openai 字段才生效,再 reopen 这条决策。
    //
    // 快照已在第 1 步拿到用户原值,restore 时能完整退回原值(包括 custom)。
    sync_root_value(&paths.config_toml, "model_provider", None)?;

    // 2c. **#212/#215 Codex 联网默认开**(Codex docs "Full access" 配对):
    // 之前 #212 用 workspace-write + network_access 真机仍弹审批弹窗 ——
    // Codex 默认 `approval_policy = OnRequest`(`protocol.rs::AskForApproval`
    // 的 `#[default] OnRequest`),sandbox 允许 ≠ 不弹窗,`is_safe_command()`
    // 把 curl 等判定"非 safe" 仍 escalate user 审批。#215 改 Codex 官方
    // 推荐的 "Full access" 配对:`sandbox_mode = danger-full-access` +
    // `approval_policy = never`,模型完全无审批 + 全部 sandbox 限制解除。
    //
    // toggle on:写 sandbox_mode + approval_policy,strip 之前 #212 可能
    //   写入的 `[sandbox_workspace_write] network_access` 残留(避免 stale
    //   entry 让 user 误以为还走 workspace-write)
    // toggle off:strip 全部三条,让 Codex 回 default(read-only + on-request)
    //
    // **Trade-off**:full-access + never 模型可读写任何文件 + 联网无审批
    // (Codex docs: "Full access means `danger-full-access` together with
    // `never`")。toggle 默认 on 接受 prompt-injection 风险换"小白开箱用",
    // 专业用户 toggle off 自己回 Codex default 沙箱。
    if cfg.direct {
        // === direct 直连(issue #317):strip sandbox/approval 私货。direct 唯一
        // 职责是写上游配置,不注入 transfer 的「全访问无审批」默认,也清掉从
        // local_proxy 切来时可能残留的这两条。Codex 回默认沙箱(read-only +
        // on-request)。
        sync_root_value(&paths.config_toml, "sandbox_mode", None)?;
        sync_root_value(&paths.config_toml, "approval_policy", None)?;
        sync_table_field(
            &paths.config_toml,
            "sandbox_workspace_write",
            "network_access",
            None,
        )?;
    } else if cfg.codex_network_access {
        sync_root_value(
            &paths.config_toml,
            "sandbox_mode",
            Some("\"danger-full-access\""),
        )?;
        sync_root_value(&paths.config_toml, "approval_policy", Some("\"never\""))?;
        sync_table_field(
            &paths.config_toml,
            "sandbox_workspace_write",
            "network_access",
            None,
        )?;
    } else {
        sync_root_value(&paths.config_toml, "sandbox_mode", None)?;
        sync_root_value(&paths.config_toml, "approval_policy", None)?;
        sync_table_field(
            &paths.config_toml,
            "sandbox_workspace_write",
            "network_access",
            None,
        )?;
    }

    // 3. config.toml: model_context_window(旧版兼容) + model_catalog_json(Codex 0.128+)
    //
    // catalog 始终写(2026-05-06):之前只在 `supports_1m=true` 时写,导致非 1M
    // provider(如 Kimi `kimi-k2.6` / MiMo `mimo-v2.5-pro`)在 Codex CLI 模型
    // 选择器里 fallback 到内置 GPT 系列名("GPT-5.5"等),用户看不到真实
    // provider/model。现在每条 provider 都通过 catalog 把 display_name 设成
    // "<provider> / <real-model>",`model_context_window` 仍只在 1M 时设。
    if cfg.direct {
        // === direct 直连(issue #317):strip model_catalog_json + 顶层 catalog
        // models + model_context_window。responses 直连用 Codex 默认 OpenAI
        // catalog(模型名同属 OpenAI 命名空间,无需 transfer 注入),这里同时
        // 清掉从 local_proxy 切来时残留的 catalog/window。
        sync_root_value(&paths.config_toml, CODEX_MODEL_CATALOG_KEY, None)?;
        clear_catalog_models(&paths.model_catalog_json)?;
        sync_root_value(&paths.config_toml, "model_context_window", None)?;
    } else {
        let catalog_literal = toml_string_literal(&paths.model_catalog_json.display().to_string());
        sync_root_value(
            &paths.config_toml,
            CODEX_MODEL_CATALOG_KEY,
            Some(&catalog_literal),
        )?;
        let models = catalog_models_for_provider(
            cfg.provider_name,
            cfg.default_model,
            cfg.supports_1m,
            cfg.model_mappings,
            cfg.model_capabilities,
        );
        upsert_catalog_models(&paths.model_catalog_json, &models)?;
        if cfg.supports_1m {
            sync_root_value(&paths.config_toml, "model_context_window", Some("1000000"))?;
        } else {
            sync_root_value(&paths.config_toml, "model_context_window", None)?;
        }
    }

    // 4. auth.json: auth_mode + OPENAI_API_KEY
    let mut auth = read_auth(&paths.auth_json)?;
    let obj = auth.as_object_mut().expect("read_auth 保证返回 Object");
    if cfg.gateway_api_key.is_empty() {
        obj.remove("OPENAI_API_KEY");
    } else {
        obj.insert(
            "auth_mode".into(),
            serde_json::Value::String("apikey".into()),
        );
        obj.insert(
            "OPENAI_API_KEY".into(),
            serde_json::Value::String(cfg.gateway_api_key.to_owned()),
        );
    }
    write_auth(&paths.auth_json, &auth)?;

    // 5. Codex Desktop UI 偏好:ensure `local-conversation-status-section-visible`
    // 跟 user 设置一致(默认 true,圆环可见)。详见 #258 真因:0.132+ 版本该
    // atom 默认 false,新装/升级 user 看不到 context 圆环,我们通过这个写
    // restore-friendly 的 single-atom-key 操作把 UI 偏好同步成 user 期望值。
    //
    // **Race 提醒**:Codex Desktop 跑着的时候 in-memory atom 会在它下次 persist
    // 时把我们的写入**覆盖**回去。所以这步**只在 Codex Desktop 启动前**有效,
    // 跟 transfer "先 apply 再启动 desktop" 的时序天然配合;如果 user 在 Codex
    // 跑着的时候切 provider,新值要重启 Codex 才生效。
    //
    // **best-effort**(silent-failure-hunter HIGH #3):atom write 在 step 1-4
    // (snapshot + config.toml + auth.json)都成功后才跑。如果失败 → propagate
    // 会让 apply 整体报 Err,但 config.toml / auth.json 已写,partial-apply 错误
    // 框架。改成 warn + 继续,跟 restore-side 对称(UI preference 失败不该 block
    // 主路径)。失败的 user 可手动 `/status` toggle 修正。
    // direct 模式不碰 status-section atom(UI 偏好;issue #317:既不写也不强清,
    // 留用户原值)。
    if !cfg.direct {
        if let Err(e) = crate::electron_state::write_atom(
            &paths.electron_global_state,
            crate::electron_state::STATUS_SECTION_VISIBLE_KEY,
            serde_json::Value::Bool(cfg.codex_status_section_default_visible),
        ) {
            tracing::warn!(
                target: "codex_integration::apply",
                path = %paths.electron_global_state.display(),
                error = %e,
                "best-effort status-section atom write failed; provider apply continues. \
                 User can manually toggle via `/status` slash command in Codex Desktop.",
            );
        }
    }

    Ok(ApplyResult {
        config_toml_path: paths.config_toml.display().to_string(),
        auth_json_path: paths.auth_json.display().to_string(),
        snapshot_taken: snapshot_taken_now,
        model_context_window_set: cfg.supports_1m && !cfg.direct,
        model_catalog_json_set: !cfg.direct,
    })
}

/// 基于快照精确还原我们改过的 key,不动用户在我们运行期间手加的内容。
/// 还原成功后清掉快照。
pub fn restore_codex_state(paths: &CodexPaths) -> Result<bool, CodexError> {
    if !has_snapshot(paths) {
        // 没快照时退化为旧版"删除我们的 key"逻辑,与 Python 行为对齐。
        //
        // ⚠️ **layered defense 注意(防回归)**:`desktop_clear` handler
        // (src-tauri desktop.rs:910) 已在 has_snapshot=false 时**先 noop
        // 返回**不调本函数,守门 follow-up #28(用户从未 apply 但手写过
        // ~/.codex/config.toml managed key 时不应被清)。**不要**因为
        // "外层已 guard 这里 fallback 是 dead code"就 DRY 删掉本分支 ——
        // 其他 caller (测试 / 其它 endpoint / 未来新 handler) 仍可能直
        // 接调 restore_codex_state,本兜底保持 Python 行为兼容。
        clear_managed_codex_state(paths)?;
        return Ok(false);
    }

    let snapshot_config = read_snapshot_config(paths).unwrap_or_default();
    let snapshot_auth = read_snapshot_auth(paths);
    restore_from_snapshot_values(paths, &snapshot_config, &snapshot_auth, RestoreMode::Auto)?;

    // 先读 manifest 拿到 atom pre-value(下面 drop_snapshot 后就读不到了),
    // **best-effort** atom restore — UI preference 失败不 block 主路径。
    let manifest = crate::snapshot::read_current_manifest(paths);

    drop_snapshot(paths)?;
    clear_catalog_models(&paths.model_catalog_json)?;

    if let Err(e) = restore_status_section_from_manifest(paths, manifest) {
        tracing::warn!(
            target: "codex_integration::apply",
            error = %e,
            "best-effort status-section atom restore failed; config/auth already restored, \
             snapshot dropped — restore success reported overall",
        );
    }

    Ok(true)
}

/// 区分两种 restore 流程:
/// - `Auto`:stop app 自动 restore。快照里没有 `model` 时保留当前 CLI 写入的活跃
///   选择(避免擦掉用户用 Codex CLI picker 选过的模型)。
/// - `Manual`:UI 手动选某个 snapshot 恢复。语义是"完全回到那个快照的状态",
///   `model` 也必须严格按快照恢复(没有就移除)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreMode {
    Auto,
    Manual,
}

/// 人工恢复指定快照。恢复成功后默认删除该快照;当 `drop_remaining_snapshots`
/// 为 true 时,按 UI 选择恢复语义清理所有剩余 active/recovery/legacy 快照。
pub fn restore_codex_snapshot(
    paths: &CodexPaths,
    snapshot_id: &str,
    drop_remaining_snapshots: bool,
) -> Result<bool, CodexError> {
    if snapshot_id.trim().is_empty() {
        return restore_codex_state(paths);
    }
    if !list_snapshots(paths).iter().any(|s| s.id == snapshot_id) {
        return Err(CodexError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("snapshot not found: {snapshot_id}"),
        )));
    }
    let snapshot_config = read_snapshot_config_by_id(paths, snapshot_id).unwrap_or_default();
    let snapshot_auth = read_snapshot_auth_by_id(paths, snapshot_id);
    restore_from_snapshot_values(paths, &snapshot_config, &snapshot_auth, RestoreMode::Manual)?;

    // 先读 manifest 拿 atom pre-value(drop 后读不到),best-effort 适用同
    // restore_codex_state。
    let manifest = crate::snapshot::read_manifest_by_id(paths, snapshot_id);

    if drop_remaining_snapshots {
        drop_all_snapshots(paths)?;
    } else {
        drop_snapshot_by_id(paths, snapshot_id)?;
    }
    clear_catalog_models(&paths.model_catalog_json)?;

    if let Err(e) = restore_status_section_from_manifest(paths, manifest) {
        tracing::warn!(
            target: "codex_integration::apply",
            error = %e,
            "best-effort status-section atom restore (manual snapshot pick) failed; \
             config/auth already restored, snapshots dropped — restore success reported overall",
        );
    }

    Ok(true)
}

/// 把 `~/.codex/.codex-global-state.json` 里
/// `electron-persisted-atom-state.local-conversation-status-section-visible`
/// 退回到 snapshot 拍摄时的原值。
///
/// **三态语义**(配合 `SnapshotManifest.electron_status_section_capture_failed`):
/// - `capture_failed=true` → snapshot 时读 atom 失败 → **不动**(避免 silently 抹
///   user 真实原值;tracing::warn! 记一条 audit trail)
/// - `capture_failed=false` + `pre_value=Some(b)` → 原值就是 b,write_atom 复原
/// - `capture_failed=false` + `pre_value=None` → atom 原本不存在,remove_atom 物理删除
/// - `manifest=None`(没快照 / 损坏 manifest)→ capture_failed=true 行为(不动)
fn restore_status_section_from_manifest(
    paths: &CodexPaths,
    manifest: Option<crate::snapshot::SnapshotManifest>,
) -> Result<(), CodexError> {
    let Some(m) = manifest else {
        tracing::warn!(
            target: "codex_integration::apply",
            "no manifest for status-section atom restore; skipping to avoid silent loss",
        );
        return Ok(());
    };
    if m.electron_status_section_capture_failed {
        tracing::warn!(
            target: "codex_integration::apply",
            "snapshot manifest marked status-section atom capture as failed; \
             skipping restore to avoid silent loss of user's real original value",
        );
        return Ok(());
    }
    // Devin Review BUG-002 fix:pre-v3 manifest 没有 atom 追踪字段(serde default
    // 给 pre_value=None + capture_failed=false),那时的 transfer 根本没管这个
    // atom,user 的 `/status` 偏好是他自己设的。restore 时**不动**避免误删。
    if m.schema_version < 3 {
        tracing::warn!(
            target: "codex_integration::apply",
            schema_version = m.schema_version,
            "pre-v3 manifest did not track status-section atom; skipping atom restore \
             to avoid silently removing user's current value",
        );
        return Ok(());
    }
    match m.electron_status_section_pre_value {
        Some(value) => crate::electron_state::write_atom(
            &paths.electron_global_state,
            crate::electron_state::STATUS_SECTION_VISIBLE_KEY,
            serde_json::Value::Bool(value),
        ),
        None => crate::electron_state::remove_atom(
            &paths.electron_global_state,
            crate::electron_state::STATUS_SECTION_VISIBLE_KEY,
        ),
    }
}

fn clear_managed_codex_state(paths: &CodexPaths) -> Result<(), CodexError> {
    for key in MANAGED_TOML_KEYS {
        sync_root_value(&paths.config_toml, key, None)?;
    }
    for (section, key) in MANAGED_TOML_TABLE_FIELDS {
        sync_table_field(&paths.config_toml, section, key, None)?;
    }
    clear_catalog_models(&paths.model_catalog_json)?;
    if paths.auth_json.exists() {
        let mut auth = read_auth(&paths.auth_json)?;
        if let Some(obj) = auth.as_object_mut() {
            for key in MANAGED_AUTH_KEYS {
                obj.remove(*key);
            }
        }
        write_auth(&paths.auth_json, &auth)?;
    }
    // 没快照 fallback 路径:**不动** electron_global_state 的 atom 字段。
    //
    // Why: 用户全局规则"不主动进行任何破坏性降级"(feedback_no_silent_destructive_fallback)。
    // 走到这里说明 transfer 从未 apply 过(没 snapshot),user 的 atom 值就是
    // user 自己 manually 设的(包括 `/status` 手动 toggle,或者新装从未碰过)。
    // strip 掉等于擅自抹掉 user 配置,违反规则。
    //
    // Trade-off: 如果上版本 transfer 写过 atom 但未生成新 schema 的 snapshot,
    // 这里清不掉残留 — 接受这个边缘 case 换"绝不擅动 user 偏好"的稳态。
    //
    // **silent-failure-hunter HIGH #5**:explicit log 让 upgrade-stale 场景留 audit
    // trail。检测当前 atom 是否存在,有则告诉 user 怎么手动重置。
    let current = crate::electron_state::read_atom(
        &paths.electron_global_state,
        crate::electron_state::STATUS_SECTION_VISIBLE_KEY,
    );
    if let Ok(Some(_)) = current {
        tracing::warn!(
            target: "codex_integration::apply",
            "no snapshot but managed atom key still present in .codex-global-state.json \
             (likely from older transfer version) — leaving in place per no-destructive-fallback rule. \
             User can manually reset via `/status` slash command in Codex Desktop.",
        );
    }
    Ok(())
}

fn restore_from_snapshot_values(
    paths: &CodexPaths,
    snapshot_config: &str,
    snapshot_auth: &serde_json::Value,
    mode: RestoreMode,
) -> Result<(), CodexError> {
    // 1. config.toml:对每个 managed key 用快照里的字面量还原;快照里没有就删。
    //
    // `model` 在 `RestoreMode::Auto`(stop app 自动 restore)下是例外:apply 不写
    // 它,但用户在 app 接管期间可能通过 Codex CLI 模型选择器选过模型,CLI 会把
    // 选择 `model = "..."` 写回 config.toml。若快照里没有 `model`,自动 restore
    // 不应擦掉用户的活跃选择,只在快照里有时还原回原值。
    //
    // `RestoreMode::Manual`(UI 手动选某个 snapshot 恢复)的语义是"完全回到那个
    // 快照的状态",所以 `model` 也必须严格按快照恢复 —— 没有就移除,否则用户选
    // 老备份反而沿用了 post-snapshot 的 model 映射。
    //
    // **#270 防污染循环固化**:若 snapshot 本身已经被 transfer apply 污染过
    // (模型选项 `model_catalog_json` 值指向 `<app_home>/config.json`,或
    // `openai_base_url` 指向 transfer proxy `127.0.0.1:18080`),按字面量回写
    // 等于把残留污染再写一遍。这里**先过一遍 signature 检测**,命中的字段
    // 视为快照里"没有"(literal = None,走 strip 分支),拒绝把 transfer 自己
    // 的产物当成"用户原始配置"恢复。详见
    // [`crate::residual::signature_fields_to_strip`]。
    //
    // 端口列表只包含历史默认 18080 — 用户改过 proxy port 后的 snapshot 这里
    // 不识别(filter 不到),会回写老 port 的 transfer 字段;此时#268 设置页
    // 「针对性清除」可以做最终 cleanup(扫描用 settings.proxyPort + 18080
    // 两个 port 都会查)。
    let polluted_fields: std::collections::HashSet<String> =
        crate::residual::signature_fields_to_strip(
            snapshot_config,
            &paths.model_catalog_json,
            &[18080],
        )
        .into_iter()
        .collect();

    for key in MANAGED_TOML_KEYS {
        let literal_from_snapshot = snapshot_toml_value_literal(snapshot_config, key);
        let literal = if polluted_fields.contains(*key) {
            None
        } else {
            literal_from_snapshot
        };
        match (*key, literal.as_deref(), mode) {
            ("model", None, RestoreMode::Auto) => continue,
            _ => sync_root_value(&paths.config_toml, key, literal.as_deref())?,
        }
    }

    // #212:table-form managed 字段,从快照对应 section body 还原字面量;
    // 快照里没有(用户原本没配 sandbox 段)→ 删 key,保留 section
    // (用户其它 key 可能还在,详见 `sync_table_field` doc)。
    for (section, key) in MANAGED_TOML_TABLE_FIELDS {
        let literal = snapshot_table_field_literal(snapshot_config, section, key);
        sync_table_field(&paths.config_toml, section, key, literal.as_deref())?;
    }

    // 2. auth.json:对每个 managed key,快照里有就改回快照值,没有就 remove
    let mut current = read_auth(&paths.auth_json)?;
    if let Some(obj) = current.as_object_mut() {
        for key in MANAGED_AUTH_KEYS {
            match snapshot_auth.get(*key) {
                Some(v) => {
                    obj.insert((*key).to_owned(), v.clone());
                }
                None => {
                    obj.remove(*key);
                }
            }
        }
    }
    write_auth(&paths.auth_json, &current)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, CodexPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(tmp.path());
        (tmp, paths)
    }

    fn read_toml(paths: &CodexPaths) -> String {
        std::fs::read_to_string(&paths.config_toml).unwrap()
    }

    fn read_auth_value(paths: &CodexPaths) -> serde_json::Value {
        read_auth(&paths.auth_json).unwrap()
    }

    fn read_app_config(paths: &CodexPaths) -> serde_json::Value {
        codex_app_transfer_registry::load_raw_config(&paths.model_catalog_json).unwrap()
    }

    #[test]
    fn apply_on_empty_writes_both_files_and_takes_snapshot() {
        let (_t, paths) = setup();
        let result = apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v2.0.0-stage2.5",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        assert!(result.snapshot_taken);
        assert!(!result.model_context_window_set);
        // catalog 现在始终写(让非 1M provider 也能在 Codex CLI 模型选择器
        // 显示"<provider> / <real-model>"而不是 fallback 到 GPT 内置名)
        assert!(result.model_catalog_json_set);

        let toml = read_toml(&paths);
        assert!(toml.contains("openai_base_url = \"http://127.0.0.1:18080\""));
        assert!(!toml.contains("model_context_window"));
        // model_catalog_json 始终在 config.toml 里
        assert!(toml.contains("model_catalog_json"));
        // #215: codex_network_access=true 写 danger-full-access + never
        //(Codex docs "Full access" 配对,真正无审批弹窗联网)
        assert!(toml.contains("sandbox_mode = \"danger-full-access\""));
        assert!(toml.contains("approval_policy = \"never\""));
        // strip 之前 #212 可能写过的 workspace_write.network_access(stale)
        assert!(!toml.contains("network_access"));

        let auth = read_auth_value(&paths);
        assert_eq!(auth["auth_mode"], "apikey");
        assert_eq!(auth["OPENAI_API_KEY"], "cas_test");
    }

    /// #212 covering test:**toggle off 时 strip 两条**(sandbox_mode +
    /// network_access),让 Codex 回 default read-only。不能像之前 explicit
    /// 写 false —— 单留 false 仍可能让 sandbox_mode 残留 workspace-write,
    /// 跟 toggle off 的语义("回原默认 sandbox")不一致。
    #[test]
    fn apply_with_network_access_false_strips_both_keys() {
        let (_t, paths) = setup();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: false,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let toml = read_toml(&paths);
        assert!(
            !toml.contains("sandbox_mode"),
            "toggle off 应 strip sandbox_mode(回 Codex default read-only): {toml}"
        );
        assert!(
            !toml.contains("approval_policy"),
            "toggle off 应 strip approval_policy(回 Codex default on-request): {toml}"
        );
        assert!(
            !toml.contains("network_access"),
            "toggle off 应 strip network_access: {toml}"
        );
    }

    /// #212 防 BLOCKER 回归:apply 后 config.toml 必须可被 `toml` crate 正常
    /// parse;如果未来谁改回 root-level dotted key 形式跟用户原 [section]
    /// 并存,会触发 duplicate table 让此测试 fail。
    #[test]
    fn apply_output_parses_with_pre_existing_sandbox_section() {
        let (_t, paths) = setup();
        // 模拟用户已显式配 [sandbox_workspace_write] 段(Codex docs 推荐形式)
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "model_provider = \"openai\"\n\n[sandbox_workspace_write]\nexclude_tmpdir_env_var = false\nexclude_slash_tmp = false\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let toml_str = read_toml(&paths);
        // 必须可 parse(无 duplicate table)
        let parsed: toml::Value =
            toml::from_str(&toml_str).expect("output 必须是合法 TOML, 否则 Codex CLI 加载会失败");
        // #215: 写 root-level danger-full-access + never,不再 touch
        // [sandbox_workspace_write] section,用户原 section + keys 完整保留
        assert_eq!(
            parsed.get("sandbox_mode").and_then(|v| v.as_str()),
            Some("danger-full-access")
        );
        assert_eq!(
            parsed.get("approval_policy").and_then(|v| v.as_str()),
            Some("never")
        );
        let section = parsed
            .get("sandbox_workspace_write")
            .and_then(|v| v.as_table())
            .expect("用户原 section 必保留");
        // network_access 既不在(没启 workspace-write 路径)
        assert!(section.get("network_access").is_none());
        assert_eq!(
            section
                .get("exclude_tmpdir_env_var")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            section.get("exclude_slash_tmp").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    /// #215 restore round-trip:apply (toggle on) 写 sandbox_mode + approval_policy
    /// + strip network_access,restore 后**全部三条 managed 都 strip**,
    /// 用户原 section header + 其它 keys 完整保留。
    #[test]
    fn restore_strips_managed_keys_keeps_user_section() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        // 用户原本只有 sandbox section + 其它 key,**没有** network_access
        std::fs::write(
            &paths.config_toml,
            "[sandbox_workspace_write]\nexclude_tmpdir_env_var = false\n",
        )
        .unwrap();
        codex_app_transfer_registry::save_raw_config(
            &paths.model_catalog_json,
            &json!({"providers": []}),
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let after_apply = read_toml(&paths);
        assert!(after_apply.contains("sandbox_mode = \"danger-full-access\""));
        assert!(after_apply.contains("approval_policy = \"never\""));
        assert!(
            !after_apply.contains("network_access"),
            "apply (toggle on) 不应写 network_access (走 full-access 路径不需要): {after_apply}"
        );
        // restore 应去掉 sandbox_mode + approval_policy,保留 section + 用户原 key
        restore_codex_state(&paths).unwrap();
        let restored = read_toml(&paths);
        assert!(
            !restored.contains("sandbox_mode"),
            "restore 应 strip sandbox_mode: {restored}"
        );
        assert!(
            !restored.contains("approval_policy"),
            "restore 应 strip approval_policy: {restored}"
        );
        assert!(
            restored.contains("[sandbox_workspace_write]"),
            "section header 必须保留: {restored}"
        );
        assert!(
            restored.contains("exclude_tmpdir_env_var = false"),
            "用户原 key 必须保留: {restored}"
        );
    }

    /// #212 Devin BLOCKER 防回归:用户原 config 用 **root-level dotted key 形式**
    /// `sandbox_workspace_write.network_access = false`(合法 TOML 等价形式)→
    /// snapshot read 路径必须识别此形式,否则 restore 返 None → caller 误删
    /// 用户原行 → 用户 security 设置永久丢失。
    #[test]
    fn restore_preserves_user_dotted_root_form_network_access() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        // 用户用 dotted root 形式显式配 false(合法 TOML)
        std::fs::write(
            &paths.config_toml,
            "sandbox_workspace_write.network_access = false\n",
        )
        .unwrap();
        codex_app_transfer_registry::save_raw_config(
            &paths.model_catalog_json,
            &json!({"providers": []}),
        )
        .unwrap();
        // #215: apply (toggle on) strip 用户原 dotted network_access(走
        // full-access 路径),restore 必须恢复用户原 false,**不**永久丢失
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let after_apply = read_toml(&paths);
        assert!(
            !after_apply.contains("network_access"),
            "apply (toggle on full-access) 应 strip 用户原 dotted network_access: {after_apply}"
        );
        // restore 必须恢复用户原 false 语义(不论是 dotted form 还是 section
        // form,TOML 两种等价 —— 当前 restore impl 走 section form 写回,
        // 关键是 value=false 恢复了,**不**永久丢失)
        restore_codex_state(&paths).unwrap();
        let restored = read_toml(&paths);
        let parsed: toml::Value =
            toml::from_str(&restored).expect("restored output 必须是合法 TOML");
        let actual = parsed
            .get("sandbox_workspace_write")
            .and_then(|v| v.get("network_access"))
            .and_then(|v| v.as_bool());
        assert_eq!(
            actual,
            Some(false),
            "restore 必须恢复用户原 network_access=false 语义: {restored}"
        );
        assert_eq!(
            restored.matches("network_access").count(),
            1,
            "只一行 network_access: {restored}"
        );
    }

    /// #212 restore round-trip:快照里**有** network_access(用户原显式配过) →
    /// restore 后恢复用户原值(不被我们的 default-on 污染)。
    #[test]
    fn restore_brings_back_user_network_access_value() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        // 用户原本显式配了 network_access = false(出于安全考虑)
        std::fs::write(
            &paths.config_toml,
            "[sandbox_workspace_write]\nnetwork_access = false\n",
        )
        .unwrap();
        codex_app_transfer_registry::save_raw_config(
            &paths.model_catalog_json,
            &json!({"providers": []}),
        )
        .unwrap();
        // #215: apply (toggle on) strip 用户原 section network_access
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let after_apply = read_toml(&paths);
        assert!(
            !after_apply.contains("network_access"),
            "apply (toggle on full-access) 应 strip 用户原 section network_access: {after_apply}"
        );
        // restore 应恢复 false
        restore_codex_state(&paths).unwrap();
        let restored = read_toml(&paths);
        assert!(
            restored.contains("network_access = false"),
            "restore 应恢复用户显式配的 false: {restored}"
        );
        assert_eq!(
            restored.matches("network_access").count(),
            1,
            "唯一一行 network_access: {restored}"
        );
    }

    #[test]
    fn apply_with_supports_1m_writes_model_context_window_and_catalog() {
        let (_t, paths) = setup();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "k",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro[1m]",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let toml = read_toml(&paths);
        assert!(toml.contains("model_context_window = 1000000"));
        assert!(toml.contains("model_catalog_json = "));
        assert!(toml.contains(".codex-app-transfer"));
        assert!(toml.contains("config.json"));
        let catalog: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&paths.model_catalog_json).unwrap()).unwrap();
        assert_eq!(catalog["models"][0]["context_window"], 1_000_000);
        assert_eq!(catalog["models"][0]["effective_context_window_percent"], 95);
        assert!(catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["slug"] == "deepseek-v4-pro"));
    }

    #[test]
    fn apply_with_supports_1m_uses_provider_slot_mapping() {
        let (_t, paths) = setup();
        let mappings = json!({
            "default": "deepseek-v4-pro",
            "gpt_5_5": "short-context-model",
            "gpt_5_4": "custom-long-model"
        });
        let capabilities = json!({
            "short-context-model": {"supports1m": false},
            "custom-long-model": {"supports1m": true}
        });

        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "k",
                supports_1m: true,
                provider_name: "Mixed",
                default_model: "deepseek-v4-pro",
                model_mappings: Some(&mappings),
                model_capabilities: Some(&capabilities),
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();

        let catalog = read_app_config(&paths);
        let models = catalog["models"].as_array().unwrap();
        let gpt55 = models.iter().find(|m| m["slug"] == "gpt-5.5").unwrap();
        let gpt54 = models.iter().find(|m| m["slug"] == "gpt-5.4").unwrap();
        let mini = models.iter().find(|m| m["slug"] == "gpt-5.4-mini").unwrap();
        // user feedback (2026-05-26): display_name 不含 "Provider / " 前缀,
        // provider 移到 description 里
        assert_eq!(gpt55["display_name"], "short-context-model");
        assert_eq!(gpt55["context_window"], 258_400);
        assert!(gpt55["description"].as_str().unwrap().contains("(Mixed)"));
        assert_eq!(gpt54["display_name"], "custom-long-model");
        assert_eq!(gpt54["context_window"], 1_000_000);
        assert_eq!(
            mini["display_name"], "deepseek-v4-pro",
            "empty slots should document their default fallback target"
        );
        assert_eq!(mini["context_window"], 1_000_000);
    }

    #[test]
    fn apply_without_supports_1m_keeps_catalog_drops_only_context_window() {
        let (_t, paths) = setup();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "k",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        assert!(read_app_config(&paths).get("models").is_some());

        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "k",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();

        // 现在 catalog 始终写,即使 supports_1m=false 也保留(2026-05-06):
        // - model_context_window 仍按 supports_1m 切换:这条只在 1M 时设
        // - model_catalog_json 与顶层 "models" 数组不再被清掉,Codex CLI
        //   能继续从 catalog 读到正确的 "<provider> / <real-model>" 显示
        let toml = read_toml(&paths);
        assert!(!toml.contains("model_context_window = "));
        assert!(toml.contains(CODEX_MODEL_CATALOG_KEY));
        let models = read_app_config(&paths)
            .get("models")
            .and_then(|v| v.as_array())
            .cloned()
            .expect("models 数组应保留");
        assert!(
            !models.is_empty(),
            "catalog 始终写,至少包含 default 模型条目"
        );
    }

    #[test]
    fn apply_preserves_user_other_toml_and_auth_fields() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "# my comment\napi_key = \"k\"\n[profiles]\nfoo = 1\n",
        )
        .unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"tokens\":{\"access\":\"xyz\"},\"OPENAI_API_KEY\":\"old\"}\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://up",
                gateway_api_key: "cas_new",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let toml = read_toml(&paths);
        assert!(toml.contains("# my comment"));
        assert!(toml.contains("api_key = \"k\""));
        assert!(toml.contains("openai_base_url = \"http://up\""));
        assert!(toml.contains("[profiles]"));
        assert!(toml.contains("foo = 1"));
        let auth = read_auth_value(&paths);
        assert_eq!(auth["OPENAI_API_KEY"], "cas_new");
        assert_eq!(auth["tokens"]["access"], "xyz", "用户 tokens 不应被动");
    }

    #[test]
    fn restore_with_snapshot_brings_back_original_values() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        // 用户原本的状态:有 base_url 和 auth.OPENAI_API_KEY
        std::fs::write(
            &paths.config_toml,
            "openai_base_url = \"https://api.openai.com/v1\"\nmodel = \"gpt-5.5\"\n[profiles]\nfoo = 1\n",
        )
        .unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"OPENAI_API_KEY\":\"sk-original\",\"tokens\":{\"a\":1}}\n",
        )
        .unwrap();
        // apply 我们的代理配置
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_proxy",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        // 模拟 Codex 在接管期间把 UI 模型选择写成第三方映射模型。
        sync_root_value(&paths.config_toml, "model", Some("\"deepseek-v4-pro\"")).unwrap();
        // 还原
        let restored = restore_codex_state(&paths).unwrap();
        assert!(restored, "有快照时 restore 应返回 true");

        let toml = read_toml(&paths);
        assert!(
            toml.contains("openai_base_url = \"https://api.openai.com/v1\""),
            "base_url 应还原为原始 OpenAI 地址"
        );
        assert!(
            !toml.contains("model_context_window"),
            "原状态没有 1M 字段,还原后也不应有"
        );
        assert!(
            toml.contains("model = \"gpt-5.5\""),
            "Codex 模型选择应还原为用户原值"
        );
        assert!(toml.contains("[profiles]"), "用户的 [profiles] 应保留");

        let auth = read_auth_value(&paths);
        assert_eq!(auth["OPENAI_API_KEY"], "sk-original");
        assert_eq!(auth["tokens"]["a"], 1);
        assert!(
            auth.get("auth_mode").is_none(),
            "原状态没有 auth_mode,还原后应不存在"
        );

        assert!(!has_snapshot(&paths), "restore 完成后应清掉快照");
        assert!(
            read_app_config(&paths).get("models").is_none(),
            "restore 应清理本应用写入的顶层 catalog models"
        );
    }

    #[test]
    fn restore_with_snapshot_restores_user_model_catalog_json_key() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "model_catalog_json = \"/tmp/user-catalog.json\"\n",
        )
        .unwrap();

        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_proxy",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        assert!(read_toml(&paths).contains(".codex-app-transfer"));
        assert!(read_app_config(&paths).get("models").is_some());

        restore_codex_state(&paths).unwrap();

        let toml = read_toml(&paths);
        assert!(toml.contains("model_catalog_json = \"/tmp/user-catalog.json\""));
        assert!(read_app_config(&paths).get("models").is_none());
    }

    /// **#270 防回归**:snapshot 自身被 transfer apply 污染过(`model_catalog_json`
    /// 指向 app_home,`openai_base_url` 指向 127.0.0.1:18080)的场景,
    /// `restore_from_snapshot_values` 必须**不**把这些字段从 snapshot 字面量
    /// 回写到 live config — 否则就是 #268 描述的循环固化 bug。
    ///
    /// 期望:filter 命中 catalog + base_url signature → 整组 5 个字段当 None
    /// 处理(strip),live config 保持干净。
    #[test]
    fn restore_does_not_write_back_polluted_signature_fields_from_snapshot() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::create_dir_all(&paths.app_home).unwrap();
        std::fs::create_dir_all(&paths.snapshot_dir).unwrap();

        // 模拟"已污染的快照":snapshot 自带 transfer 写过的全套字段
        let polluted_snapshot = format!(
            "personality = \"pragmatic\"\nopenai_base_url = \"http://127.0.0.1:18080\"\nmodel_context_window = 1000000\nmodel_catalog_json = \"{}\"\nmodel = \"gpt-5.5\"\nsandbox_mode = \"danger-full-access\"\napproval_policy = \"never\"\n",
            paths.model_catalog_json.display()
        );
        std::fs::write(&paths.snapshot_config, &polluted_snapshot).unwrap();
        std::fs::write(&paths.snapshot_auth, "{}").unwrap();
        // legacy 单 snapshot 路径(用 `paths.snapshot_manifest` 让 has_snapshot=true)
        std::fs::write(
            &paths.snapshot_manifest,
            r#"{"snapshot_id":"legacy","session_id":"legacy","schema_version":1}"#,
        )
        .unwrap();

        // live config 也含污染(模拟"apply 中"的状态)
        std::fs::write(&paths.config_toml, &polluted_snapshot).unwrap();
        // 给 auth.json 一个空对象避免 read 失败
        std::fs::write(&paths.auth_json, "{}").unwrap();

        restore_codex_state(&paths).unwrap();

        let toml = read_toml(&paths);
        // 用户合法字段保留
        assert!(
            toml.contains("personality = \"pragmatic\""),
            "user 自有 key 必须保留: {toml}"
        );
        assert!(
            toml.contains("model = \"gpt-5.5\""),
            "user 的 model 必须保留: {toml}"
        );
        // 5 个 transfer signature 字段全 strip(即便快照里有)
        for k in [
            "openai_base_url",
            "model_context_window",
            "model_catalog_json",
            "sandbox_mode",
            "approval_policy",
        ] {
            assert!(
                !toml.contains(&format!("{k} =")),
                "snapshot 已污染时 transfer 字段 {k} 必须被 filter 而不是回写: {toml}"
            );
        }
    }

    #[test]
    fn restore_without_snapshot_falls_back_to_remove_managed_keys() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "openai_base_url = \"http://leftover\"\nmodel_context_window = 1000000\nmodel_catalog_json = \"leftover.json\"\nmodel = \"deepseek-v4-pro\"\nmodel_provider = \"codex-app-transfer\"\nfoo = 1\n",
        )
        .unwrap();
        codex_app_transfer_registry::save_raw_config(
            &paths.model_catalog_json,
            &json!({
                "version": "1.0.4",
                "models": [{"slug": "gpt-5.5"}],
                "settings": {"theme": "default"}
            }),
        )
        .unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"leftover\",\"keep\":1}\n",
        )
        .unwrap();
        let restored = restore_codex_state(&paths).unwrap();
        assert!(!restored, "没有快照时返回 false");
        let toml = read_toml(&paths);
        assert!(!toml.contains("openai_base_url"));
        assert!(!toml.contains("model_context_window"));
        assert!(!toml.contains(CODEX_MODEL_CATALOG_KEY));
        assert!(!toml.contains("model = "));
        assert!(!toml.contains("model_provider = "));
        assert!(toml.contains("foo = 1"));
        assert!(read_app_config(&paths).get("models").is_none());
        let auth = read_auth_value(&paths);
        assert!(auth.get("OPENAI_API_KEY").is_none());
        assert!(auth.get("auth_mode").is_none());
        assert_eq!(auth["keep"], 1);
    }

    #[test]
    fn restore_snapshot_by_id_restores_chosen_backup_and_cleans_all_snapshots() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "openai_base_url = \"active-original\"\nmodel = \"gpt-5.5\"\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://active-managed",
                gateway_api_key: "cas_active",
                supports_1m: false,
                provider_name: "Active",
                default_model: "active-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v-active",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();

        let recovery_dir = paths.recovery_snapshots_dir.join("older-backup");
        std::fs::create_dir_all(&recovery_dir).unwrap();
        std::fs::write(
            recovery_dir.join("config.toml"),
            "openai_base_url = \"older-original\"\nmodel = \"gpt-5.4\"\n",
        )
        .unwrap();
        std::fs::write(recovery_dir.join("auth.json"), "{\"keep\":1}\n").unwrap();
        std::fs::write(
            recovery_dir.join("manifest.json"),
            json!({
                "schema_version": 2,
                "snapshot_id": "older-backup",
                "session_id": "older-session",
                "snapshot_at": "2026-05-15T02:00:00",
                "config_existed": true,
                "auth_existed": true,
                "app_version": "v-old",
                "provider_name": "Older"
            })
            .to_string(),
        )
        .unwrap();

        sync_root_value(
            &paths.config_toml,
            "openai_base_url",
            Some("\"http://managed\""),
        )
        .unwrap();
        sync_root_value(&paths.config_toml, "model", Some("\"deepseek-v4-pro\"")).unwrap();

        let restored = restore_codex_snapshot(&paths, "older-backup", true).unwrap();
        assert!(restored);
        let toml = read_toml(&paths);
        assert!(toml.contains("openai_base_url = \"older-original\""));
        assert!(toml.contains("model = \"gpt-5.4\""));
        assert!(
            crate::snapshot::list_snapshots(&paths).is_empty(),
            "manual restore should clear all remaining backups after success"
        );
    }

    #[test]
    fn apply_then_apply_again_does_not_overwrite_original_snapshot() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"original\"\n").unwrap();
        // 第一次 apply
        let r1 = apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://first",
                gateway_api_key: "cas_first",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        assert!(r1.snapshot_taken);
        // 第二次 apply
        let r2 = apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://second",
                gateway_api_key: "cas_second",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        assert!(!r2.snapshot_taken, "第二次不应再 snapshot");
        // restore 应回到 ORIGINAL,不是 first
        restore_codex_state(&paths).unwrap();
        let toml = read_toml(&paths);
        assert!(toml.contains("openai_base_url = \"original\""));
    }

    #[test]
    fn apply_with_empty_gateway_api_key_removes_key() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"OPENAI_API_KEY\":\"present\",\"keep\":1}\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let auth = read_auth_value(&paths);
        assert!(auth.get("OPENAI_API_KEY").is_none());
        assert_eq!(auth["keep"], 1);
    }

    #[test]
    fn apply_with_empty_base_url_removes_key() {
        let (_t, paths) = setup();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "",
                gateway_api_key: "k",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let toml = std::fs::read_to_string(&paths.config_toml).unwrap_or_default();
        assert!(!toml.contains("openai_base_url"));
    }

    /// 防回归:若用户的 config.toml 里某 key 含 `key_alt = ...` 这种前缀同名行,
    /// apply / restore 都不应误改它(已由 toml_sync 单测覆盖,这里再做端到端校验)。
    #[test]
    fn similar_prefixed_keys_are_not_touched() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "openai_base_url_alt = \"keep\"\nopenai_base_url = \"old\"\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://new",
                gateway_api_key: "k",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let toml = read_toml(&paths);
        assert!(toml.contains("openai_base_url_alt = \"keep\""));
        assert!(toml.contains("openai_base_url = \"http://new\""));
    }

    #[test]
    fn auth_json_unaffected_when_user_has_oauth_tokens() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        let oauth_blob = json!({
            "tokens": {
                "access_token": "ya29.xxx",
                "refresh_token": "1//xxx",
                "expires_at": 9999999999i64
            }
        });
        std::fs::write(
            &paths.auth_json,
            serde_json::to_string_pretty(&oauth_blob).unwrap(),
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let auth = read_auth_value(&paths);
        assert_eq!(auth["tokens"]["access_token"], "ya29.xxx");
        assert_eq!(auth["OPENAI_API_KEY"], "cas_test");
        // restore 应把 OAuth 块完整保留,把 OPENAI_API_KEY 删除(原来没有)
        restore_codex_state(&paths).unwrap();
        let auth_after = read_auth_value(&paths);
        assert_eq!(auth_after["tokens"]["access_token"], "ya29.xxx");
        assert!(auth_after.get("OPENAI_API_KEY").is_none());
        assert!(auth_after.get("auth_mode").is_none());
    }

    /// #258:apply 无条件 strip `model_provider` 字段 —— Codex CLI 缺失时 fallback
    /// 到 openai 跟显式写等价,strip 避开"显式字段触发上游 UI surface" 的潜在路径。
    /// 注:这意味着 user 旧 config 残留 `model_provider = "custom"` 也会被 strip,
    /// 走 CLI default openai —— 跟 #178 强制覆盖逻辑等价(都不会让流量进入
    /// `[model_providers.custom]` 段),但 footprint 更小。无 env opt-in 写回:
    /// 任何允许 model_provider 字段写回 config 的 escape hatch 都可能让"残留
    /// model_provider 导致回话丢失"风险复发,所以 strip 是终态。
    #[test]
    fn apply_strips_legacy_custom_model_provider() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            concat!(
                "model_provider = \"custom\"\n",
                "openai_base_url = \"https://stale.example.com/v1\"\n",
                "[model_providers.custom]\n",
                "name = \"Custom\"\n",
                "base_url = \"https://stale.example.com/v1\"\n",
            ),
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let toml = read_toml(&paths);
        assert!(
            !toml.contains("model_provider ="),
            "apply 应 strip model_provider 字段,实际 toml:\n{toml}"
        );
        assert!(
            toml.contains("openai_base_url = \"http://127.0.0.1:18080\""),
            "openai_base_url 应指向 app proxy"
        );
        assert!(
            toml.contains("[model_providers.custom]"),
            "[model_providers.custom] 不是我们管的段,保留即可"
        );

        // restore 必须把 model_provider 退回到用户原值 "custom"。
        restore_codex_state(&paths).unwrap();
        let restored = read_toml(&paths);
        assert!(
            restored.contains("model_provider = \"custom\""),
            "restore 应把 model_provider 退回为用户原值,实际 toml:\n{restored}"
        );
        assert!(
            restored.contains("openai_base_url = \"https://stale.example.com/v1\""),
            "openai_base_url 也应退回用户原值"
        );
    }

    /// UI 手动选某个 snapshot 恢复时,语义是"完全回到那个快照的状态"。即使快照里
    /// 没有 `model`,也必须把当前 `model` 移除(否则用户选老备份反而沿用了
    /// post-snapshot 的 model 映射)。RestoreMode::Auto 才保留 CLI 写入的选择,
    /// Manual 不应享受这个例外。
    #[test]
    fn manual_restore_strictly_matches_snapshot_even_for_model_key() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"original\"\n").unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        // 模拟接管期间 CLI picker 写入的活跃 model。
        sync_root_value(&paths.config_toml, "model", Some("\"deepseek-v4-pro\"")).unwrap();

        // 拿到 active snapshot id,走手动恢复路径。
        let snapshots = crate::snapshot::list_snapshots(&paths);
        let snapshot_id = snapshots
            .iter()
            .find(|s| s.kind == "active")
            .expect("apply 应创建 active snapshot")
            .id
            .clone();
        restore_codex_snapshot(&paths, &snapshot_id, false).unwrap();

        let toml = read_toml(&paths);
        assert!(
            !toml.contains("model = "),
            "manual restore 必须严格按快照恢复;快照无 model 时应移除当前值,实际 toml:\n{toml}"
        );
        assert!(
            toml.contains("openai_base_url = \"original\""),
            "openai_base_url 也按快照退回"
        );
    }

    /// 用户首次安装时 config.toml 没有 `model`,apply 也不写 `model`。但用户在
    /// Codex CLI 模型选择器里选过模型后,CLI 会把 `model = "..."` 写回 config.toml。
    /// restore 时快照里没有 `model`,我们不应把 CLI 写入的活跃选择擦掉。
    #[test]
    fn restore_preserves_user_model_picked_via_codex_cli() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "openai_base_url = \"https://api.openai.com/v1\"\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_proxy",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        // 模拟 Codex CLI picker 在 app 接管期间把 model 写回 config.toml。
        sync_root_value(&paths.config_toml, "model", Some("\"kimi-k2.6\"")).unwrap();

        restore_codex_state(&paths).unwrap();
        let toml = read_toml(&paths);
        assert!(
            toml.contains("model = \"kimi-k2.6\""),
            "快照里没有 model 时,restore 应保留 CLI 写入的活跃选择,实际 toml:\n{toml}"
        );
    }

    /// #258:apply 把 `local-conversation-status-section-visible` ensure 成 user 设置值,
    /// restore 严格退回到 snapshot 拍摄时的原值(包括"原本不存在" → strip 退路)。
    #[test]
    fn apply_writes_status_section_atom_and_restore_reverts_to_snapshot_original() {
        let (_t, paths) = setup();
        let global_state = &paths.electron_global_state;

        // 模拟 user 原本完全没设过该 atom(文件压根不存在)
        assert!(!global_state.exists());

        // apply with default_visible=true (transfer 默认行为)
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        // atom 被写入 true
        assert_eq!(
            crate::electron_state::read_atom(
                global_state,
                crate::electron_state::STATUS_SECTION_VISIBLE_KEY,
            )
            .unwrap(),
            Some(json!(true))
        );

        // restore:user 原本无此 atom → restore 应 strip,文件里该 key 应消失
        restore_codex_state(&paths).unwrap();
        assert_eq!(
            crate::electron_state::read_atom(
                global_state,
                crate::electron_state::STATUS_SECTION_VISIBLE_KEY,
            )
            .unwrap(),
            None,
            "restore 必须 strip 我们写入的 atom(因为 snapshot 拍到 user 原本没此字段)"
        );
    }

    /// Devin Review BUG-002 防回归:pre-v3 manifest(schema_version=2)由旧 transfer
    /// 写,根本没追踪 atom。restore 必须**不动** atom 避免误删 user 升级后的手动设置。
    #[test]
    fn restore_does_not_touch_atom_for_pre_v3_manifest() {
        let (_t, paths) = setup();
        let global_state = &paths.electron_global_state;

        // 模拟 user 升级前手动 toggle 过 `/status` → atom=true
        std::fs::create_dir_all(global_state.parent().unwrap()).unwrap();
        std::fs::write(
            global_state,
            r#"{"electron-persisted-atom-state":{"local-conversation-status-section-visible":true}}"#,
        )
        .unwrap();

        // 模拟旧 transfer (schema v2) 的 manifest:没 atom 字段
        let pre_v3_manifest = crate::snapshot::SnapshotManifest {
            schema_version: 2,
            snapshot_id: "legacy".into(),
            session_id: "legacy".into(),
            snapshot_at: "2026-05-01T00:00:00".into(),
            config_existed: false,
            auth_existed: false,
            app_version: "v-old".into(),
            provider_name: None,
            // serde default 模拟:旧 manifest 没这俩字段
            electron_status_section_pre_value: None,
            electron_status_section_capture_failed: false,
        };

        restore_status_section_from_manifest(&paths, Some(pre_v3_manifest)).unwrap();

        // user 的 atom=true 必须**不被删** —— 旧 transfer 没管它,新版 restore 也不该删
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(global_state).unwrap()).unwrap();
        assert_eq!(
            after.pointer(
                "/electron-persisted-atom-state/local-conversation-status-section-visible"
            ),
            Some(&json!(true)),
            "pre-v3 manifest restore 必须不动 atom(旧 transfer 没追踪此字段,user 手动设置不该被删)"
        );
    }

    /// Devin Review BUG-003 防回归:snapshot 时 atom 是 non-boolean 值(手改 / 未来
    /// Codex Desktop 改格式)→ snapshot 必须记 capture_failed=true,restore 必须不动。
    #[test]
    fn snapshot_marks_capture_failed_when_atom_is_not_boolean() {
        let (_t, paths) = setup();
        let global_state = &paths.electron_global_state;

        // 模拟 user atom 是非 boolean (手改或未来 schema 变化)
        std::fs::create_dir_all(global_state.parent().unwrap()).unwrap();
        std::fs::write(
            global_state,
            r#"{"electron-persisted-atom-state":{"local-conversation-status-section-visible":"yes"}}"#,
        )
        .unwrap();

        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();

        let manifest =
            crate::snapshot::read_current_manifest(&paths).expect("snapshot 应已写 manifest");
        assert!(
            manifest.electron_status_section_capture_failed,
            "non-boolean atom 必须 mark capture_failed=true(否则 restore 会误删)"
        );

        // restore 必须**不动** atom — apply 写入的 true 留在那(没原值可退回)
        restore_codex_state(&paths).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(global_state).unwrap()).unwrap();
        // 关键:atom 字段没被 remove(restore short-circuit 跳过 atom touching)
        assert!(
            after
                .pointer("/electron-persisted-atom-state/local-conversation-status-section-visible")
                .is_some(),
            "capture_failed=true 时 restore 必须不调 remove_atom,atom 字段必须保留"
        );
    }

    /// silent-failure-hunter CRITICAL #1 防回归:snapshot 时 `.codex-global-state.json`
    /// 损坏 → snapshot 应记 capture_failed=true,restore **不动** atom 防 silent 抹原值。
    #[test]
    fn restore_does_not_touch_atom_when_snapshot_capture_failed() {
        let (_t, paths) = setup();
        let global_state = &paths.electron_global_state;

        // 模拟 user 真实场景:.codex-global-state.json 内容损坏(可能 mid-write 崩),
        // 但里面 ATOM_STATE 段实际上仍有 atom=true(我们看不到,因为整体非法 JSON)。
        std::fs::create_dir_all(global_state.parent().unwrap()).unwrap();
        std::fs::write(global_state, "{ corrupt json garbage").unwrap();

        // apply 应**不 panic**(best-effort + tracing warn 即可)
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .expect("apply must not fail when atom file is corrupt — UI preference best-effort");

        // 验证 manifest 标了 capture_failed
        let manifest =
            crate::snapshot::read_current_manifest(&paths).expect("snapshot 应已写 manifest");
        assert!(
            manifest.electron_status_section_capture_failed,
            "snapshot 必须把 capture 失败信号写进 manifest"
        );
        assert_eq!(
            manifest.electron_status_section_pre_value, None,
            "capture failed 时 pre_value 必须留 None"
        );

        // restore 应不动文件(保持 corrupt 原样,让 user 自己处理),并且不 panic
        restore_codex_state(&paths).unwrap();
        assert_eq!(
            std::fs::read_to_string(global_state).unwrap(),
            "{ corrupt json garbage",
            "restore 必须不动损坏文件(防 silently 抹 user 真实原 atom 值)"
        );
    }

    /// #258:user **原本**已经手动 toggle 过 `/status` 为 false,apply 又把它改为 true,
    /// restore 必须退回 false(尊重 user 原选择,不留 transfer 写入的 true)。
    #[test]
    fn restore_preserves_user_original_status_section_value() {
        let (_t, paths) = setup();
        let global_state = &paths.electron_global_state;

        // user 已有的 global-state 文件,手动 toggle 过 /status off,且有其它字段
        std::fs::create_dir_all(global_state.parent().unwrap()).unwrap();
        std::fs::write(
            global_state,
            r#"{"electron-saved-workspace-roots":["/tmp/proj"],"electron-persisted-atom-state":{"local-conversation-status-section-visible":false,"composer-auto-context-enabled":true}}"#,
        )
        .unwrap();

        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        // apply 覆盖成 true,其它字段保留
        let after_apply: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(global_state).unwrap()).unwrap();
        assert_eq!(
            after_apply.pointer(
                "/electron-persisted-atom-state/local-conversation-status-section-visible"
            ),
            Some(&json!(true))
        );
        assert_eq!(
            after_apply.pointer("/electron-persisted-atom-state/composer-auto-context-enabled"),
            Some(&json!(true))
        );
        assert_eq!(
            after_apply.get("electron-saved-workspace-roots"),
            Some(&json!(["/tmp/proj"]))
        );

        // restore:atom 退回 user 原值 false,其它字段仍保留
        restore_codex_state(&paths).unwrap();
        let after_restore: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(global_state).unwrap()).unwrap();
        assert_eq!(
            after_restore.pointer(
                "/electron-persisted-atom-state/local-conversation-status-section-visible"
            ),
            Some(&json!(false)),
            "restore 必须用 snapshot 记录的 user 原值 false 复原,不留 transfer 写入的 true"
        );
        assert_eq!(
            after_restore.pointer("/electron-persisted-atom-state/composer-auto-context-enabled"),
            Some(&json!(true)),
            "我们 only-touch 单 atom,user 同段其它 atom 必须原样保留"
        );
        assert_eq!(
            after_restore.get("electron-saved-workspace-roots"),
            Some(&json!(["/tmp/proj"])),
            "user 顶层字段必须原样保留"
        );
    }

    /// issue #317:direct 直连只写上游配置(openai_base_url + auth key),
    /// **strip** 全部 transfer 私货 —— 即便 supports_1m + network_access 都为
    /// true,也不写 catalog / model_context_window / sandbox_mode / approval_policy。
    #[test]
    fn apply_direct_only_writes_upstream_and_strips_private() {
        let (_t, paths) = setup();
        let result = apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "https://up.example.com/v1",
                gateway_api_key: "sk-upstream",
                supports_1m: true, // 即便 1M,direct 也不写 window
                provider_name: "Custom",
                default_model: "gpt-5.5",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true, // 即便 on,direct 也不写 sandbox
                codex_status_section_default_visible: true,
                direct: true,
            },
        )
        .unwrap();
        assert!(!result.model_context_window_set, "direct 不应设 window");
        assert!(!result.model_catalog_json_set, "direct 不应设 catalog");

        let toml = read_toml(&paths);
        assert!(
            toml.contains("openai_base_url = \"https://up.example.com/v1\""),
            "direct 必须写上游 base_url: {toml}"
        );
        assert!(
            !toml.contains("model_catalog_json"),
            "direct 不应写 catalog: {toml}"
        );
        assert!(
            !toml.contains("model_context_window"),
            "direct 不应写 window: {toml}"
        );
        assert!(
            !toml.contains("sandbox_mode"),
            "direct 不应写 sandbox: {toml}"
        );
        assert!(
            !toml.contains("approval_policy"),
            "direct 不应写 approval: {toml}"
        );

        let auth = read_auth_value(&paths);
        assert_eq!(auth["OPENAI_API_KEY"], "sk-upstream");
        assert_eq!(auth["auth_mode"], "apikey");
    }

    /// issue #317:从 local_proxy 切到 direct —— direct apply 的 strip 同时承担
    /// 「清掉 local_proxy 残留私货」的清理机制,切换后 config 必须干净。
    #[test]
    fn switch_local_proxy_to_direct_strips_residual_private_fields() {
        let (_t, paths) = setup();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_proxy",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: false,
            },
        )
        .unwrap();
        let after_proxy = read_toml(&paths);
        assert!(after_proxy.contains("model_catalog_json"));
        assert!(after_proxy.contains("model_context_window = 1000000"));
        assert!(after_proxy.contains("sandbox_mode"));
        assert!(read_app_config(&paths).get("models").is_some());

        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "https://up.example.com/v1",
                gateway_api_key: "sk-upstream",
                supports_1m: true,
                provider_name: "Custom",
                default_model: "gpt-5.5",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: true,
            },
        )
        .unwrap();
        let after_direct = read_toml(&paths);
        assert!(
            after_direct.contains("openai_base_url = \"https://up.example.com/v1\""),
            "切 direct 后 base_url 必须是上游: {after_direct}"
        );
        assert!(
            !after_direct.contains("model_catalog_json"),
            "切 direct 应清 catalog: {after_direct}"
        );
        assert!(
            !after_direct.contains("model_context_window"),
            "切 direct 应清 window: {after_direct}"
        );
        assert!(
            !after_direct.contains("sandbox_mode"),
            "切 direct 应清 sandbox: {after_direct}"
        );
        assert!(
            !after_direct.contains("approval_policy"),
            "切 direct 应清 approval: {after_direct}"
        );
        assert!(
            read_app_config(&paths).get("models").is_none(),
            "切 direct 应清掉顶层 catalog models"
        );
    }

    /// issue #317:direct apply 后 restore 仍能恢复用户原始配置(复用 snapshot +
    /// MANAGED_TOML_KEYS,direct 没写的字段 strip=noop,安全)。
    #[test]
    fn direct_apply_then_restore_brings_back_user_values() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "openai_base_url = \"https://api.openai.com/v1\"\nmodel = \"gpt-5.5\"\n[profiles]\nfoo = 1\n",
        )
        .unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"OPENAI_API_KEY\":\"sk-original\",\"tokens\":{\"a\":1}}\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "https://up.example.com/v1",
                gateway_api_key: "sk-upstream",
                supports_1m: false,
                provider_name: "Custom",
                default_model: "gpt-5.5",
                model_mappings: None,
                model_capabilities: None,
                app_version: "v",
                codex_network_access: true,
                codex_status_section_default_visible: true,
                direct: true,
            },
        )
        .unwrap();
        assert!(read_toml(&paths).contains("openai_base_url = \"https://up.example.com/v1\""));

        restore_codex_state(&paths).unwrap();
        let toml = read_toml(&paths);
        assert!(
            toml.contains("openai_base_url = \"https://api.openai.com/v1\""),
            "restore 应回用户原 base_url: {toml}"
        );
        assert!(toml.contains("[profiles]"), "用户 [profiles] 应保留");
        let auth = read_auth_value(&paths);
        assert_eq!(auth["OPENAI_API_KEY"], "sk-original");
    }
}
