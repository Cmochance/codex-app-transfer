//! 首次 apply 前的快照机制.
//!
//! 1. apply 前调一次 [`snapshot_codex_state`]:把当前 `config.toml` 与
//!    `auth.json` 整文件复制到当前进程 session 的 active 快照目录,并写
//!    一份 `manifest.json` 记录"这两个文件原本存不存在"。
//! 2. 同一进程 session 内已经有 active 快照时,**不重复**(同会话多次 apply
//!    不会污染最初备份)。
//! 3. 发现旧 session active 快照时,先移动到 timestamp/session 命名的
//!    recovery 目录,再创建本 session active 快照,避免多版本/多进程启动覆盖
//!    最早的用户配置。
//! 4. restore 时基于快照精确还原我们改过的几个 key,**不动**用户在我们运行
//!    期间手加的内容。

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::paths::CodexPaths;
use crate::toml_sync::write_atomic;
use crate::CodexError;

/// v3:开始追踪 context-usage atom 的 pre-value(`electron_status_section_*` 字段)。
/// v4 [MOC-123]:追踪的 atom key 从已废 `local-conversation-status-section-visible` 换成
/// 现役 `show-context-window-usage`(见 [`crate::electron_state::CONTEXT_USAGE_ATOM_KEY`])。
/// v3 manifest 存的是**旧 key** 的 pre-value,**不能**拿去 restore 新 key —— 否则
/// `None` 会误删 user 自己设的 footer 偏好(transfer 从没 capture 过)。restore + 迁移
/// 路径据此 `< 4` 跳过 / 标 capture_failed(见 PR #360 chatgpt-codex-connector P2)。
const SNAPSHOT_SCHEMA_VERSION: u32 = 4;

/// `gc_trash_older_than` 的默认保留天数 — daemon startup 调一次时用。
/// 30 天是"误点 cleanup_all 后用户还有月内时间发现并从 trash/ 恢复"的
/// 平衡点。若未来开 UI/CLI 配置入口,改 caller 传值,这条常量做 fallback。
pub const TRASH_RETENTION_DAYS: u64 = 30;

/// recovery/ 保留上限。`move_stale_active_snapshots_to_recovery` 每次 apply 都会
/// 把上个 session 遗留的 active 快照搬进 recovery/ 当安全存档;若不封顶会无上限
/// 累积(#268 残留扫描越来越慢 + 污染样本长期留存)。保留最近 N 份足够覆盖
/// "崩溃/强退后想恢复上次原配置"的实际诉求(配合内容去重,N 份都是不同内容)。
const MAX_RECOVERY_SNAPSHOTS: usize = 5;

static CURRENT_SESSION_ID: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotManifest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub snapshot_id: String,
    #[serde(default)]
    pub session_id: String,
    pub snapshot_at: String,
    pub config_existed: bool,
    pub auth_existed: bool,
    pub app_version: String,
    #[serde(default)]
    pub provider_name: Option<String>,
    /// `~/.codex/.codex-global-state.json` 里
    /// `electron-persisted-atom-state.show-context-window-usage`
    /// (见 [`crate::electron_state::CONTEXT_USAGE_ATOM_KEY`])在 snapshot 拍摄时的原值。
    /// (字段名保留 `status_section` 作 manifest serde 向后兼容,语义现为 context-usage atom。)
    ///
    /// **三态语义**(配合 `electron_status_section_capture_failed`):
    /// - `capture_failed=false` + `Some(v)` → snapshot 时已读到该值,restore 走 write_atom 复原
    /// - `capture_failed=false` + `None` → snapshot 时确认 atom 不存在,restore 走 remove_atom
    /// - `capture_failed=true` + `None` → snapshot 时读 atom 失败(IO/parse 错误),
    ///   restore **不动** atom(避免 silently 抹掉 user 真实原值)
    ///
    /// `#[serde(default)]` 保证旧 manifest(schema_version < 3 没有此字段)
    /// deserialize 到 `None` + `capture_failed=false`,等同"原本无该字段" → restore 走 remove。
    #[serde(default)]
    pub electron_status_section_pre_value: Option<bool>,
    /// `electron_status_section_pre_value` capture 失败 sentinel。详见上面三态。
    /// 旧 manifest 默认 false(snapshot 老路径全是成功 capture,行为不变)。
    #[serde(default)]
    pub electron_status_section_capture_failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotStatus {
    pub has_snapshot: bool,
    pub snapshot_at: Option<String>,
    pub config_existed: bool,
    pub auth_existed: bool,
    pub app_version: Option<String>,
    pub restorable_count: usize,
    pub recovery_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub id: String,
    pub kind: String,
    pub snapshot_at: String,
    pub config_existed: bool,
    pub auth_existed: bool,
    pub app_version: String,
    pub provider_name: Option<String>,
    pub current_session: bool,
}

/// 是否有未还原的快照。
/// 读当前快照的 manifest(走 active / 兼容 legacy)。
/// 没快照或读失败返 `None`,caller(`apply::restore_*`)按"原本就没记录"语义走。
pub fn read_current_manifest(paths: &CodexPaths) -> Option<SnapshotManifest> {
    if let Some(dir) = current_snapshot_dir(paths) {
        return read_manifest_from_dir(&dir).ok();
    }
    None
}

/// 读指定 snapshot_id 的 manifest。
pub fn read_manifest_by_id(paths: &CodexPaths, snapshot_id: &str) -> Option<SnapshotManifest> {
    snapshot_dir_by_id(paths, snapshot_id).and_then(|dir| read_manifest_from_dir(&dir).ok())
}

pub fn has_snapshot(paths: &CodexPaths) -> bool {
    current_session_snapshot_dir(paths)
        .map(|dir| manifest_path(&dir).exists())
        .unwrap_or(false)
        || paths.snapshot_manifest.exists()
}

/// 列出人工恢复可选择的所有快照(不含敏感字段)。
pub fn list_snapshots(paths: &CodexPaths) -> Vec<SnapshotInfo> {
    let mut out = Vec::new();
    if paths.snapshot_manifest.exists() {
        if let Ok(manifest) = read_manifest_from_dir(&paths.snapshot_dir) {
            out.push(info_from_manifest(
                normalize_manifest(manifest, "legacy", "legacy"),
                "legacy",
            ));
        }
    }

    for dir in snapshot_dirs_under(&paths.active_snapshots_dir) {
        if let Ok(manifest) = read_manifest_from_dir(&dir) {
            let fallback = dir_name(&dir).unwrap_or_else(|| "active".to_owned());
            out.push(info_from_manifest(
                normalize_manifest(manifest, &fallback, &fallback),
                "active",
            ));
        }
    }

    for dir in snapshot_dirs_under(&paths.recovery_snapshots_dir) {
        if let Ok(manifest) = read_manifest_from_dir(&dir) {
            let fallback = dir_name(&dir).unwrap_or_else(|| "recovery".to_owned());
            out.push(info_from_manifest(
                normalize_manifest(manifest, &fallback, &fallback),
                "recovery",
            ));
        }
    }

    out.sort_by(|a, b| b.snapshot_at.cmp(&a.snapshot_at).then(a.kind.cmp(&b.kind)));
    out
}

/// 供 UI 展示用的快照状态(不含敏感字段)。
pub fn get_snapshot_status(paths: &CodexPaths) -> SnapshotStatus {
    let snapshots = list_snapshots(paths);
    let active = current_snapshot_info(paths);
    let recovery_count = snapshots
        .iter()
        .filter(|s| s.kind == "recovery" || s.kind == "legacy")
        .count();
    if active.is_none() {
        return SnapshotStatus {
            has_snapshot: false,
            snapshot_at: None,
            config_existed: false,
            auth_existed: false,
            app_version: None,
            restorable_count: snapshots.len(),
            recovery_count,
        };
    }
    let active = active.expect("checked above");
    SnapshotStatus {
        has_snapshot: true,
        snapshot_at: Some(active.snapshot_at),
        config_existed: active.config_existed,
        auth_existed: active.auth_existed,
        app_version: Some(active.app_version),
        restorable_count: snapshots.len(),
        recovery_count,
    }
}

/// 读当前 `~/.codex/.codex-global-state.json` 里 context-usage atom 的原值,返回
/// `(pre_value, capture_failed)`(三态语义见 [`SnapshotManifest`])。snapshot 在
/// apply 写 atom **之前**调,所以这里读到的是 user 写入前的原值。
///
/// BUG-003:atom 被手改 / 未来 Codex 改成非 boolean → `as_bool()` 返 None,不能误当
/// "原本无字段"(restore 会 remove 误删),显式 mark capture_failed 防 silent loss。
fn capture_context_usage_pre_value(paths: &CodexPaths) -> (Option<bool>, bool) {
    match crate::electron_state::read_atom(
        &paths.electron_global_state,
        crate::electron_state::CONTEXT_USAGE_ATOM_KEY,
    ) {
        Ok(Some(v)) => match v.as_bool() {
            Some(b) => (Some(b), false),
            None => {
                tracing::warn!(
                    target: "codex_integration::snapshot",
                    path = %paths.electron_global_state.display(),
                    value = %v,
                    "context-usage atom is not a boolean; marking capture as failed \
                     to avoid silent loss on restore",
                );
                (None, true)
            }
        },
        Ok(None) => (None, false),
        Err(e) => {
            tracing::warn!(
                target: "codex_integration::snapshot",
                path = %paths.electron_global_state.display(),
                error = %e,
                "snapshot pre-value capture failed for context-usage atom; \
                 restore will skip atom touching to avoid silent loss",
            );
            (None, true)
        }
    }
}

/// 复用已存在的 active / legacy snapshot manifest。若它是 `< SNAPSHOT_SCHEMA_VERSION`
/// 的旧版本(没追踪 context-usage atom,或追踪的是已废旧 key),则**重新捕获**当前
/// (apply 写 atom 之前)的 atom 原值、升到当前 schema 后回写。
///
/// [MOC-123 / PR #360 P2] 为什么必须升级而不是原样复用:apply 之后会无条件写
/// `show-context-window-usage` atom,而 restore 现在跳过所有 `< 4` manifest —— 若复用的
/// 旧 manifest 不升级,写进去的 atom 就**没有可 restore 的 pre-value**,transfer 退出
/// 时清不掉、永久留在 user global-state(破坏"退出恢复 Codex 默认")。升级前的旧
/// transfer 只写过旧 key,所以此刻读到的新 key 值就是 user 的真实原值,捕获正确。
///
/// `manage_atom=false`(direct 直连模式):transfer 不写也不该 restore 这个 atom
/// (#317 留用户原值),升级时标 capture_failed=true 让 restore 跳过。
fn reuse_manifest_upgrading_atom(
    paths: &CodexPaths,
    dir: &Path,
    manage_atom: bool,
) -> Result<SnapshotManifest, CodexError> {
    let mut manifest = read_manifest_from_dir(dir)?;
    if manifest.schema_version < SNAPSHOT_SCHEMA_VERSION {
        let (pre_value, capture_failed) = if manage_atom {
            capture_context_usage_pre_value(paths)
        } else {
            (None, true)
        };
        manifest.electron_status_section_pre_value = pre_value;
        manifest.electron_status_section_capture_failed = capture_failed;
        manifest.schema_version = SNAPSHOT_SCHEMA_VERSION;
        write_manifest_to_dir(dir, &manifest)?;
    }
    Ok(manifest)
}

/// `manage_atom`:transfer 这次 apply 是否会**写** context-usage atom(= `!cfg.direct`)。
/// direct 直连模式不写也**不 restore** 该 atom(#317:既不写也不强清,留用户原值)——
/// 否则 direct 会话期间 user 在 Codex Settings 手动改的圆环会被退出 restore 覆盖
/// (PR #360 chatgpt-codex-connector P2)。direct 时 snapshot 把 atom 标 capture_failed
/// 让 restore 跳过。
pub fn snapshot_codex_state(
    paths: &CodexPaths,
    app_version: &str,
    provider_name: &str,
    manage_atom: bool,
) -> Result<SnapshotManifest, CodexError> {
    move_stale_active_snapshots_to_recovery(paths)?;

    let current_dir = current_active_snapshot_dir(paths);
    if manifest_path(&current_dir).exists() {
        return reuse_manifest_upgrading_atom(paths, &current_dir, manage_atom);
    }
    if paths.snapshot_manifest.exists() {
        return reuse_manifest_upgrading_atom(paths, &paths.snapshot_dir, manage_atom);
    }
    std::fs::create_dir_all(&current_dir)?;

    let config_existed = paths.config_toml.exists();
    let auth_existed = paths.auth_json.exists();

    if config_existed {
        std::fs::copy(&paths.config_toml, config_path(&current_dir))?;
    }
    if auth_existed {
        let snapshot_auth = auth_path(&current_dir);
        std::fs::copy(&paths.auth_json, &snapshot_auth)?;
        // 快照里的 auth 也要 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(snapshot_auth, std::fs::Permissions::from_mode(0o600));
        }
    }

    // 读 `~/.codex/.codex-global-state.json` 里 status section atom 的原值。
    // None = 文件不存在 / 段不存在 / atom 缺失,restore 时走 remove。
    //
    // **错误处理 silent-failure-hunter CRITICAL fix**:read_atom 现在区分
    // ENOENT (Ok(None)) vs 其它 IO / parse 错误 (Err)。读错时**不能**默认 None ——
    // 否则后续 restore 用 None 当"原本无此字段" → remove_atom → silent 丢
    // user 真实原值。改成读错时:warn 一行 + manifest pre_value 保留 None,
    // 但 restore 路径会 short-circuit"不动 atom"(见 apply.rs restore 改动)。
    // 这里用 capture-failed sentinel 区分清楚两种 None。
    let (electron_status_section_pre_value, electron_status_section_capture_failed) = if manage_atom
    {
        capture_context_usage_pre_value(paths)
    } else {
        // direct 模式:transfer 不写 atom,也不该退出时 restore-revert(#317 留用户原值)。
        // 标 capture_failed → restore 跳过 → direct 会话期间 user 手动改的圆环不被覆盖。
        (None, true)
    };

    let session_id = current_session_id().to_owned();
    let manifest = SnapshotManifest {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        snapshot_id: session_id.clone(),
        session_id,
        snapshot_at: Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        config_existed,
        auth_existed,
        app_version: app_version.to_owned(),
        provider_name: if provider_name.is_empty() {
            None
        } else {
            Some(provider_name.to_owned())
        },
        electron_status_section_pre_value,
        electron_status_section_capture_failed,
    };
    write_manifest_to_dir(&current_dir, &manifest)?;

    // follow-up #30 守门: 在系统级用户数据目录额外 cp 一份冗余备份,防
    // ~/.codex-app-transfer/ 整目录被用户/卸载脚本/磁盘清理误删 → 真原始
    // 账号永久丢失。冗余备份失败 silently ignore(主路径已成功,backup 是
    // P1 enhancement 不该阻塞 apply 流程);codex_integration 无 tracing
    // dep 不能 warn,但 caller (src-tauri) 可通过比对 active_snapshots_dir
    // vs external_backup_dir 状态主动 health check 暴露 backup 失败。
    let _ = mirror_snapshot_to_external_backup(paths, &current_dir);

    Ok(manifest)
}

/// 把当前 session 的 active snapshot 镜像到系统级用户数据目录(冗余备份)。
/// 失败 silently 返 Err(主 snapshot 已成功不应 propagate)。
///
/// 镜像策略: `external_backup_dir/<session-id>/` 下放 manifest.json +
/// 可选的 config.toml / auth.json 整文件副本。已存在的同名目录直接覆盖
/// (同 session 多次 apply 幂等)。
fn mirror_snapshot_to_external_backup(
    paths: &CodexPaths,
    source_dir: &Path,
) -> Result<(), CodexError> {
    let session_id = current_session_id();
    let target_dir = paths.external_backup_dir.join(session_id);
    if target_dir.exists() {
        let _ = std::fs::remove_dir_all(&target_dir);
    }
    std::fs::create_dir_all(&target_dir)?;
    copy_dir_recursive(source_dir, &target_dir)?;
    Ok(())
}

/// 删除整个快照目录(restore 完成后的清理)。
pub fn drop_snapshot(paths: &CodexPaths) -> Result<(), CodexError> {
    if let Some(dir) = current_snapshot_dir(paths) {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// 删除指定快照。人工恢复成功后可用来清理已恢复项。
pub fn drop_snapshot_by_id(paths: &CodexPaths, snapshot_id: &str) -> Result<(), CodexError> {
    if let Some(dir) = snapshot_dir_by_id(paths, snapshot_id) {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// 软删除所有 active/recovery/legacy 快照 — **移动**到 `trash/<UTC-timestamp>/`
/// 而不是物理 `remove_dir_all`。给用户"误点 cleanup_all 还能从 trash 恢复"
/// 窗口,follow-up #29 守门防真原始账号信息被一次性删光。
///
/// trash 目录由 [`gc_trash_older_than`] 定期清理(daemon 启动调一次,默认
/// 保留 30 天)。即便 GC 不跑,trash 也只 grow 不丢历史。
///
/// 任何子 move 失败(典型场景: trash 跨文件系统不支持 rename)fallback 到
/// "copy + remove_dir_all" 保证软删除语义(数据先到 trash 再清旧位)。
pub fn drop_all_snapshots(paths: &CodexPaths) -> Result<(), CodexError> {
    let trash_bucket = paths.trash_snapshots_dir.join(format!(
        "{}-cleanup",
        Local::now().format("%Y%m%dT%H%M%S%3f")
    ));
    std::fs::create_dir_all(&trash_bucket)?;

    move_dir_to_trash(&paths.active_snapshots_dir, &trash_bucket.join("active"))?;
    move_dir_to_trash(
        &paths.recovery_snapshots_dir,
        &trash_bucket.join("recovery"),
    )?;
    move_dir_to_trash(&paths.snapshot_dir, &trash_bucket.join("legacy"))?;

    // trash_bucket 空(没东西可移)→ 清掉空目录避免日积月累空 bucket 堆积
    if trash_bucket
        .read_dir()
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(&trash_bucket);
    }
    Ok(())
}

fn move_dir_to_trash(src: &Path, dst: &Path) -> Result<(), CodexError> {
    if !src.exists() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // rename 成功直接返。失败原因(跨 FS EXDEV / 权限 EACCES / 磁盘满 ENOSPC /
    // Windows ERROR_SHARING_VIOLATION 等)在 fallback 失败时拼进 err message
    // 防丢上下文 —— 之前 `.is_ok()` 直接吞所有 rename err 让 debug 无从下手
    // (silent-failure-hunter review H1)。
    let rename_err = match std::fs::rename(src, dst) {
        Ok(_) => return Ok(()),
        Err(e) => e,
    };
    copy_dir_recursive(src, dst).map_err(|copy_err| {
        CodexError::Io(std::io::Error::other(format!(
            "move_dir_to_trash: rename failed ({rename_err}), copy fallback also failed: {copy_err}"
        )))
    })?;
    std::fs::remove_dir_all(src).map_err(|remove_err| {
        // copy 成功但 src 删失败 → trash 有副本 + src 残留,语义破。caller 应
        // 拿 err message 提示用户手动清理 src 防 has_snapshot 误判 active 仍存在
        // (silent-failure-hunter review H2)。
        CodexError::Io(std::io::Error::other(format!(
            "move_dir_to_trash: rename failed ({rename_err}), copy ok but src removal failed: {remove_err}. \
             trash 已有副本,src 残留需手动清理避免 has_snapshot 误判"
        )))
    })?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), CodexError> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// GC `trash/` 下 mtime 超过 `retention_days` 天的 bucket。
///
/// 返 `(removed, failed)` 计数:成功删的 bucket 数 + 应删但失败的 bucket 数。
/// 任何子目录 remove 失败不 propagate(GC 不是关键路径),但**计数失败**让
/// caller 能 log 区分 "trash 是空(removed=0/failed=0)" vs "GC 跑了但全
/// 失败(removed=0/failed=N)"。修 silent-failure-hunter review H3。
///
/// 入参 `retention_days` 极大值溢出 → 返 (0, 0),语义"留所有 bucket"。
///
/// 建议 caller:daemon / app 启动时调一次 `gc_trash_older_than(paths,
/// TRASH_RETENTION_DAYS)`。
pub fn gc_trash_older_than(paths: &CodexPaths, retention_days: u64) -> (usize, usize) {
    let Ok(read) = std::fs::read_dir(&paths.trash_snapshots_dir) else {
        return (0, 0);
    };
    let cutoff = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(
        retention_days.saturating_mul(86_400),
    ));
    let Some(cutoff) = cutoff else {
        return (0, 0);
    };
    let mut removed = 0usize;
    let mut failed = 0usize;
    for entry in read.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < cutoff {
            if std::fs::remove_dir_all(entry.path()).is_ok() {
                removed += 1;
            } else {
                failed += 1;
            }
        }
    }
    (removed, failed)
}

fn read_manifest_from_dir(dir: &Path) -> Result<SnapshotManifest, CodexError> {
    let s = std::fs::read_to_string(manifest_path(dir))?;
    Ok(serde_json::from_str(&s)?)
}

fn write_manifest_to_dir(dir: &Path, manifest: &SnapshotManifest) -> Result<(), CodexError> {
    let mut s = serde_json::to_string_pretty(manifest)?;
    s.push('\n');
    write_atomic(&manifest_path(dir), &s)?;
    Ok(())
}

/// 读取快照中的 config.toml 内容(不存在时返回空)。
pub(crate) fn read_snapshot_config(paths: &CodexPaths) -> Option<String> {
    current_snapshot_dir(paths).and_then(|dir| read_snapshot_config_from_dir(&dir))
}

pub(crate) fn read_snapshot_config_by_id(paths: &CodexPaths, snapshot_id: &str) -> Option<String> {
    snapshot_dir_by_id(paths, snapshot_id).and_then(|dir| read_snapshot_config_from_dir(&dir))
}

fn read_snapshot_config_from_dir(dir: &Path) -> Option<String> {
    std::fs::read_to_string(config_path(dir)).ok()
}

/// 读取快照中的 auth.json(不存在时返回空对象)。
pub(crate) fn read_snapshot_auth(paths: &CodexPaths) -> serde_json::Value {
    current_snapshot_dir(paths)
        .map(|dir| read_snapshot_auth_from_dir(&dir))
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

pub(crate) fn read_snapshot_auth_by_id(paths: &CodexPaths, snapshot_id: &str) -> serde_json::Value {
    snapshot_dir_by_id(paths, snapshot_id)
        .map(|dir| read_snapshot_auth_from_dir(&dir))
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

fn read_snapshot_auth_from_dir(dir: &Path) -> serde_json::Value {
    let txt = std::fs::read_to_string(auth_path(dir)).ok();
    match txt {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(&s)
            .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
        _ => serde_json::Value::Object(Default::default()),
    }
}

fn default_schema_version() -> u32 {
    1
}

fn current_session_id() -> &'static str {
    CURRENT_SESSION_ID
        .get_or_init(|| {
            format!(
                "{}-p{}",
                Local::now().format("%Y%m%dT%H%M%S%3f"),
                std::process::id()
            )
        })
        .as_str()
}

fn current_active_snapshot_dir(paths: &CodexPaths) -> PathBuf {
    paths.active_snapshots_dir.join(current_session_id())
}

fn current_session_snapshot_dir(paths: &CodexPaths) -> Option<PathBuf> {
    let dir = current_active_snapshot_dir(paths);
    manifest_path(&dir).exists().then_some(dir)
}

fn current_snapshot_dir(paths: &CodexPaths) -> Option<PathBuf> {
    current_session_snapshot_dir(paths).or_else(|| {
        paths
            .snapshot_manifest
            .exists()
            .then_some(paths.snapshot_dir.clone())
    })
}

fn current_snapshot_info(paths: &CodexPaths) -> Option<SnapshotInfo> {
    let dir = current_snapshot_dir(paths)?;
    let kind = if dir == paths.snapshot_dir {
        "legacy"
    } else {
        "active"
    };
    let fallback = if kind == "legacy" {
        "legacy".to_owned()
    } else {
        dir_name(&dir).unwrap_or_else(|| "active".to_owned())
    };
    let manifest = read_manifest_from_dir(&dir).ok()?;
    Some(info_from_manifest(
        normalize_manifest(manifest, &fallback, &fallback),
        kind,
    ))
}

fn snapshot_dir_by_id(paths: &CodexPaths, snapshot_id: &str) -> Option<PathBuf> {
    for dir in snapshot_dirs_under(&paths.active_snapshots_dir) {
        if snapshot_dir_matches_id(&dir, snapshot_id) {
            return Some(dir);
        }
    }
    if paths.snapshot_manifest.exists() && snapshot_id == "legacy" {
        return Some(paths.snapshot_dir.clone());
    }
    if paths.snapshot_manifest.exists() && snapshot_dir_matches_id(&paths.snapshot_dir, snapshot_id)
    {
        return Some(paths.snapshot_dir.clone());
    }
    for dir in snapshot_dirs_under(&paths.recovery_snapshots_dir) {
        if snapshot_dir_matches_id(&dir, snapshot_id) {
            return Some(dir);
        }
    }
    None
}

fn snapshot_dir_matches_id(dir: &Path, snapshot_id: &str) -> bool {
    let fallback = dir_name(dir).unwrap_or_default();
    if fallback == snapshot_id {
        return true;
    }
    read_manifest_from_dir(dir)
        .ok()
        .map(|manifest| {
            manifest.snapshot_id == snapshot_id
                || manifest.session_id == snapshot_id
                || (manifest.snapshot_id.is_empty() && snapshot_id == fallback)
        })
        .unwrap_or(false)
}

fn move_stale_active_snapshots_to_recovery(paths: &CodexPaths) -> Result<(), CodexError> {
    let current = current_session_id().to_owned();
    for dir in snapshot_dirs_under(&paths.active_snapshots_dir) {
        if dir_name(&dir).as_deref() == Some(current.as_str()) {
            continue;
        }
        move_snapshot_dir_to_recovery(paths, &dir)?;
    }
    // 每次 apply 顺手封顶 recovery/(也清理修复前积压的历史无上限存量)。
    // best-effort:纯 GC,失败不冒泡阻断 apply(见 prune_recovery_snapshots)。
    prune_recovery_snapshots(paths);
    Ok(())
}

/// 读单个文件用于去重比对,**区分"文件不存在"和"读失败"**:
/// - 不存在(`NotFound`)→ `Some(None)`(合法的"空内容")
/// - 存在且读成功 → `Some(Some(s))`
/// - 存在但读失败(I/O / 权限)→ `None`(内容**不可判定**)
///
/// 这是 BUG-fix(MOC-148 review IMPORTANT#2):旧实现用 `.ok()` 把读失败和
/// 不存在都折叠成 `None`,极端下会让"内容其实不同但读失败"的 stale active 被
/// 误判为与某份"空"备份重复 → 删掉唯一副本(违反"不主动破坏性降级")。
fn read_dedup_field(path: &Path) -> Option<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Some(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

/// 快照的 atom restore 等价类(去重指纹的一部分)。restore 行为只由 manifest 的
/// `(capture_failed, pre_value)` 三态 + `schema_version` 决定:
/// - `capture_failed=true`(或 `schema_version < 4` 兜底、manifest 读不出)→ restore
///   **不动** atom → 归一为 [`AtomRestoreKey::Untouched`];
/// - 否则按 `pre_value` 复原(`Some(v)` 写入 / `None` 移除)→ [`AtomRestoreKey::Restore`]。
///
/// config/auth 相同但本键不同的两份 restore 结果不同、**不可互换**,不能去重(MOC-148
/// review P2:否则删"重复"时会丢失只存在于 manifest 的 atom 恢复值)。
#[derive(Debug, Clone, PartialEq, Eq)]
enum AtomRestoreKey {
    /// restore 不动 atom。
    Untouched,
    /// restore 按该 `pre_value` 复原(`Some(v)` 写入 / `None` 移除)。
    Restore(Option<bool>),
}

/// 读 `dir` 的 manifest 归一成 [`AtomRestoreKey`]。口径与 [`move_snapshot_dir_to_recovery`]
/// 写 recovery manifest 一致(`schema_version < 4` 视作 capture_failed)。manifest 读不出
/// (不存在 / IO / parse 错误)→ restore 兜底"不动 atom" → [`AtomRestoreKey::Untouched`]。
fn snapshot_atom_restore_key(dir: &Path) -> AtomRestoreKey {
    match read_manifest_from_dir(dir) {
        Ok(m) if !m.electron_status_section_capture_failed && m.schema_version >= 4 => {
            AtomRestoreKey::Restore(m.electron_status_section_pre_value)
        }
        _ => AtomRestoreKey::Untouched,
    }
}

/// 读快照 dir 的去重指纹:config.toml + auth.json 内容 + atom restore 等价键。
/// **不含 manifest 的 timestamp/session_id/snapshot_id**(每份都不同),但**含**影响
/// restore 的 atom 三态(见 [`AtomRestoreKey`])。
/// config/auth 任一"存在但读失败"→ 返回 `None`(内容不可判定,调用方据此保守不去重)。
fn snapshot_content_for_dedup(
    dir: &Path,
) -> Option<(Option<String>, Option<String>, AtomRestoreKey)> {
    let config = read_dedup_field(&config_path(dir))?;
    let auth = read_dedup_field(&auth_path(dir))?;
    Some((config, auth, snapshot_atom_restore_key(dir)))
}

/// recovery/ 是否已存在与 `dir` 内容(config + auth + atom restore 等价键)完全相同的备份。
///
/// 保守语义:`dir` 自身 config/auth 读不出(不可判定)→ 直接返回 `false`(当作非重复、
/// 保留),绝不因"读不出"去删唯一副本;某份 recovery 读不出 → 该份不参与匹配。
/// atom restore 键即使 manifest 读不出也有确定兜底(`Untouched`),始终参与比较。
fn recovery_has_content_duplicate(paths: &CodexPaths, dir: &Path) -> bool {
    let Some(target) = snapshot_content_for_dedup(dir) else {
        return false;
    };
    snapshot_dirs_under(&paths.recovery_snapshots_dir)
        .iter()
        .any(|rec| snapshot_content_for_dedup(rec).as_ref() == Some(&target))
}

/// recovery/ 只保留最近 [`MAX_RECOVERY_SNAPSHOTS`] 份,其余物理删除。
///
/// 目录名形如 `20260603T210740197-pNNNN`,固定宽度时间戳前缀 → 字典序即时间序;
/// 按名降序排,保留最新的前 N 份。
///
/// **best-effort**(MOC-148 review IMPORTANT#1):这是纯 GC,单个目录删失败
/// (并发进程占用 / 权限 / 半删残留)只 warn 跳过,**绝不冒泡**——否则会让
/// 调用链顶端的 `apply_provider` 整体失败(快照本身已成功,失败的只是清旧)。
/// 与同文件 `gc_trash_older_than` 既有约定一致。
fn prune_recovery_snapshots(paths: &CodexPaths) {
    let mut dirs = snapshot_dirs_under(&paths.recovery_snapshots_dir);
    if dirs.len() <= MAX_RECOVERY_SNAPSHOTS {
        return;
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(dir_name(d)));
    for dir in dirs.into_iter().skip(MAX_RECOVERY_SNAPSHOTS) {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(
                target: "codex_integration::snapshot",
                dir = %dir.display(),
                error = %e,
                "prune recovery snapshot failed; skipping (best-effort GC)",
            );
        }
    }
}

fn move_snapshot_dir_to_recovery(paths: &CodexPaths, dir: &Path) -> Result<(), CodexError> {
    // 备份去重:recovery/ 已有内容(config+auth)相同的备份时,直接丢弃这份 stale
    // active,不再存一份重复内容(用户要求:备份时字段比对,不留重复)。
    if recovery_has_content_duplicate(paths, dir) {
        std::fs::remove_dir_all(dir)?;
        return Ok(());
    }

    let fallback = dir_name(dir).unwrap_or_else(|| current_session_id().to_owned());
    let manifest = read_manifest_from_dir(dir)
        .map(|m| normalize_manifest(m, &fallback, &fallback))
        .unwrap_or_else(|_| SnapshotManifest {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: fallback.clone(),
            session_id: fallback.clone(),
            snapshot_at: Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            config_existed: config_path(dir).exists(),
            auth_existed: auth_path(dir).exists(),
            app_version: String::new(),
            provider_name: None,
            // 损坏快照的兜底 manifest 没原值信息。capture_failed=true 让 restore
            // 安全 short-circuit"不动 atom"(防 silent 抹 user 原值)。
            electron_status_section_pre_value: None,
            electron_status_section_capture_failed: true,
        });
    std::fs::create_dir_all(&paths.recovery_snapshots_dir)?;
    let target = unique_recovery_dir(paths, &manifest.snapshot_id);
    std::fs::rename(dir, &target)?;
    // BUG-004 fix + [MOC-123] v4 bump:升级 schema_version 到当前版本前先存原版本。
    // 原版本 `< 4` 的 manifest 要么没追踪 atom(pre-v3),要么追踪的是已废旧 key
    // `local-conversation-status-section-visible`(v3)—— 升 schema 后必须**同时**设
    // capture_failed=true,否则 manual restore 把(旧 key 的 / 缺失的)pre_value 当成
    // 现役 `show-context-window-usage` 的权威值 → None 时 remove_atom 误删 user 自己设的
    // footer 偏好(见 apply.rs restore guard 设计)。
    let original_schema = manifest.schema_version;
    let mut recovery_manifest = manifest;
    recovery_manifest.schema_version = SNAPSHOT_SCHEMA_VERSION;
    if original_schema < 4 {
        recovery_manifest.electron_status_section_capture_failed = true;
    }
    if let Some(target_id) = dir_name(&target) {
        recovery_manifest.snapshot_id = target_id;
    }
    write_manifest_to_dir(&target, &recovery_manifest)?;
    Ok(())
}

fn unique_recovery_dir(paths: &CodexPaths, snapshot_id: &str) -> PathBuf {
    let safe = sanitize_path_segment(snapshot_id);
    let mut candidate = paths.recovery_snapshots_dir.join(&safe);
    let mut idx = 2;
    while candidate.exists() {
        candidate = paths.recovery_snapshots_dir.join(format!("{safe}-{idx}"));
        idx += 1;
    }
    candidate
}

fn sanitize_path_segment(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        Local::now().format("%Y%m%dT%H%M%S%3f").to_string()
    } else {
        trimmed.to_owned()
    }
}

fn snapshot_dirs_under(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && manifest_path(path).exists())
        .collect()
}

fn info_from_manifest(manifest: SnapshotManifest, kind: &str) -> SnapshotInfo {
    SnapshotInfo {
        id: manifest.snapshot_id,
        kind: kind.to_owned(),
        snapshot_at: manifest.snapshot_at,
        config_existed: manifest.config_existed,
        auth_existed: manifest.auth_existed,
        app_version: manifest.app_version,
        provider_name: manifest.provider_name,
        current_session: manifest.session_id == current_session_id(),
    }
}

fn normalize_manifest(
    mut manifest: SnapshotManifest,
    fallback_id: &str,
    fallback_session_id: &str,
) -> SnapshotManifest {
    if manifest.schema_version == 0 {
        manifest.schema_version = default_schema_version();
    }
    if manifest.snapshot_id.is_empty() {
        manifest.snapshot_id = fallback_id.to_owned();
    }
    if manifest.session_id.is_empty() {
        manifest.session_id = fallback_session_id.to_owned();
    }
    manifest
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.toml")
}

fn auth_path(dir: &Path) -> PathBuf {
    dir.join("auth.json")
}

fn dir_name(dir: &Path) -> Option<String> {
    dir.file_name()
        .map(|name| name.to_string_lossy().to_string())
}

/// 解析快照 config.toml 中某个 root key 的原始字面量(包含引号等)。
/// 返回 `None` 表示快照里**没有**这个 key,`Some(literal)` 表示快照里此 key
/// 的字面量(可能包含两侧引号、整数无引号等);该字面量可直接喂回
/// [`crate::toml_sync::sync_root_value`]。
pub(crate) fn snapshot_toml_value_literal(content: &str, key: &str) -> Option<String> {
    extract_literal_in_lines(content.lines(), key)
}

/// 解析快照 config.toml 中 `[section]` table 内某个 key 的字面量。
///
/// **读写对称**(2026-05-19 Devin BLOCKER 修):跟 `sync_table_field_in_memory`
/// 对称识别两种合法 TOML 形式:
/// 1. root-level dotted key `<section>.<key> = ...`
/// 2. `[section]` table 内 `<key> = ...`
///
/// 之前只识别形式 2 → 若用户原 config 用形式 1 写,snapshot lookup 返 None
/// → restore 把用户原行当作"没有"误删 → 用户原 security 设置丢失。
///
/// section header 匹配跟 `sync_table_field` 一致兼容尾部 `# comment`。
pub(crate) fn snapshot_table_field_literal(
    content: &str,
    section: &str,
    key: &str,
) -> Option<String> {
    // 形式 1:dotted root key 优先(若用户原 config 这么写,直接返字面量)
    let dotted_key = format!("{section}.{key}");
    if let result @ Some(_) = snapshot_toml_value_literal(content, &dotted_key) {
        return result;
    }

    // 形式 2:`[section]` table body 内查找
    let header = format!("[{section}]");
    let lines: Vec<&str> = content.lines().collect();
    let start = lines
        .iter()
        .position(|l| matches_section_header(l, &header))?;
    let mut end = lines.len();
    for (offset, line) in lines.iter().enumerate().skip(start + 1) {
        if line.trim_start().starts_with('[') {
            end = offset;
            break;
        }
    }
    extract_literal_in_lines(lines[start + 1..end].iter().copied(), key)
}

/// section header 匹配:精确 `[section]` 或带尾部 `#` 注释。
/// 与 `toml_sync::matches_section_header` 行为对称(跟 sync_table_field 同步)。
fn matches_section_header(line: &str, header: &str) -> bool {
    let trimmed = line.trim();
    if trimmed == header {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix(header) {
        let rest = rest.trim_start();
        return rest.is_empty() || rest.starts_with('#');
    }
    false
}

fn extract_literal_in_lines<'a, I: Iterator<Item = &'a str>>(
    lines: I,
    key: &str,
) -> Option<String> {
    for line in lines {
        let stripped = line.trim_start();
        if !stripped.starts_with(key) {
            continue;
        }
        let after = &stripped[key.len()..];
        // 必须是 `key=...` 或 `key <空白> ...=...` 形式
        let mut rest = after.trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        rest = rest[1..].trim_start();
        // 去掉行末注释(`# ...`)
        if let Some(idx) = rest.find('#') {
            rest = rest[..idx].trim_end();
        }
        let trimmed = rest.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        return Some(trimmed.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_with_tmp() -> (tempfile::TempDir, CodexPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(tmp.path());
        (tmp, paths)
    }

    #[test]
    fn drop_all_snapshots_moves_to_trash_not_physical_delete() {
        // follow-up #29 守门:cleanup_all=true 不能物理删 active/recovery,
        // 必须移到 trash/ 保留恢复窗口。
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"x\"\n").unwrap();
        std::fs::write(&paths.auth_json, "{\"OPENAI_API_KEY\":\"sk-real\"}\n").unwrap();
        snapshot_codex_state(&paths, "v-test", "Mock", true).unwrap();
        assert!(has_snapshot(&paths));

        drop_all_snapshots(&paths).unwrap();

        // active/ 已被 move 走
        assert!(!has_snapshot(&paths));
        // trash/ 下应有一个 <timestamp>-cleanup bucket,内含 active 子目录
        let trash_buckets: Vec<_> = std::fs::read_dir(&paths.trash_snapshots_dir)
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(trash_buckets.len(), 1, "trash 应该有 1 个 bucket");
        let bucket = trash_buckets[0].path();
        assert!(
            bucket.join("active").exists(),
            "active 应被 move 到 trash/<bucket>/active"
        );
    }

    #[test]
    fn drop_all_snapshots_when_nothing_to_move_does_not_create_empty_bucket() {
        // 三个目录都不存在时不应留空 bucket 在 trash/
        let (_t, paths) = paths_with_tmp();
        drop_all_snapshots(&paths).unwrap();
        let trash_count = std::fs::read_dir(&paths.trash_snapshots_dir)
            .map(|it| it.flatten().count())
            .unwrap_or(0);
        assert_eq!(trash_count, 0, "trash 不应有空 bucket");
    }

    #[test]
    fn gc_trash_removes_old_buckets_keeps_fresh() {
        // GC 按 mtime 区分新旧 bucket
        use std::time::{Duration, SystemTime};
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.trash_snapshots_dir).unwrap();
        let old = paths.trash_snapshots_dir.join("20200101T000000000-cleanup");
        let fresh = paths.trash_snapshots_dir.join("20260517T000000000-cleanup");
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&fresh).unwrap();

        // 把 old 的 mtime 设到 100 天前,fresh 保持当前
        let ancient = SystemTime::now() - Duration::from_secs(100 * 86_400);
        let f = std::fs::File::open(&old).unwrap();
        f.set_modified(ancient).unwrap();
        drop(f);

        let (removed, failed) = gc_trash_older_than(&paths, 30);
        assert_eq!(removed, 1, "应该清掉 1 个 100 天老 bucket");
        assert_eq!(failed, 0, "无 remove 失败");
        assert!(!old.exists(), "old bucket 应已被清");
        assert!(fresh.exists(), "fresh bucket 必须保留");
    }

    #[test]
    fn snapshot_when_neither_file_exists() {
        let (_t, paths) = paths_with_tmp();
        let m = snapshot_codex_state(&paths, "v2.0.0-stage2.5", "Mock", true).unwrap();
        assert!(!m.config_existed);
        assert!(!m.auth_existed);
        assert!(has_snapshot(&paths));
        assert!(read_snapshot_config(&paths).is_none());
        assert_eq!(
            read_snapshot_auth(&paths),
            serde_json::Value::Object(Default::default())
        );
    }

    #[test]
    fn snapshot_copies_existing_files() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"existing\"\n").unwrap();
        std::fs::write(&paths.auth_json, "{\"OPENAI_API_KEY\":\"existing\"}\n").unwrap();
        let m = snapshot_codex_state(&paths, "v", "Mock", true).unwrap();
        assert!(m.config_existed);
        assert!(m.auth_existed);
        assert_eq!(
            read_snapshot_config(&paths).unwrap(),
            "openai_base_url = \"existing\"\n"
        );
    }

    #[test]
    fn snapshot_is_idempotent() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "old\n").unwrap();
        snapshot_codex_state(&paths, "v", "Mock", true).unwrap();
        // 改了 config.toml,再 snapshot 一次 —— 不应覆盖原始备份
        std::fs::write(&paths.config_toml, "new\n").unwrap();
        snapshot_codex_state(&paths, "v", "Mock", true).unwrap();
        assert_eq!(
            read_snapshot_config(&paths).unwrap(),
            "old\n",
            "首次快照后再次调用必须保留原始备份"
        );
    }

    #[test]
    fn drop_snapshot_clears_dir() {
        let (_t, paths) = paths_with_tmp();
        snapshot_codex_state(&paths, "v", "Mock", true).unwrap();
        assert!(has_snapshot(&paths));
        drop_snapshot(&paths).unwrap();
        assert!(!has_snapshot(&paths));
    }

    #[test]
    fn stale_active_snapshot_moves_to_recovery_before_new_snapshot() {
        let (_t, paths) = paths_with_tmp();
        let stale_dir = paths.active_snapshots_dir.join("old-session");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(config_path(&stale_dir), "openai_base_url = \"original\"\n").unwrap();
        std::fs::write(
            manifest_path(&stale_dir),
            serde_json::to_string(&SnapshotManifest {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
                snapshot_id: "old-session".to_owned(),
                session_id: "old-session".to_owned(),
                snapshot_at: "2026-05-15T01:00:00".to_owned(),
                config_existed: true,
                auth_existed: false,
                app_version: "v-old".to_owned(),
                provider_name: Some("Old".to_owned()),
                electron_status_section_pre_value: None,
                electron_status_section_capture_failed: false,
            })
            .unwrap(),
        )
        .unwrap();

        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"current\"\n").unwrap();
        snapshot_codex_state(&paths, "v-new", "New", true).unwrap();

        assert!(!stale_dir.exists());
        let snapshots = list_snapshots(&paths);
        assert!(snapshots
            .iter()
            .any(|s| s.kind == "active" && s.app_version == "v-new"));
        assert!(snapshots
            .iter()
            .any(|s| s.kind == "recovery" && s.id == "old-session"));
    }

    // ── MOC-148 搭车:recovery/ 去重 + 上限 ────────────────────────────

    fn mk_manifest(id: &str) -> SnapshotManifest {
        SnapshotManifest {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            snapshot_id: id.to_owned(),
            session_id: id.to_owned(),
            snapshot_at: "2026-06-01T00:00:00".to_owned(),
            config_existed: true,
            auth_existed: false,
            app_version: "v".to_owned(),
            provider_name: None,
            electron_status_section_pre_value: None,
            electron_status_section_capture_failed: false,
        }
    }

    fn seed_recovery(paths: &CodexPaths, name: &str, config: &str, auth: Option<&str>) -> PathBuf {
        let dir = paths.recovery_snapshots_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(config_path(&dir), config).unwrap();
        if let Some(a) = auth {
            std::fs::write(auth_path(&dir), a).unwrap();
        }
        write_manifest_to_dir(&dir, &mk_manifest(name)).unwrap();
        dir
    }

    /// 备份去重:recovery/ 已有内容(config+auth)相同的备份时,stale active 被丢弃,
    /// 不再新增一份重复内容。
    #[test]
    fn move_to_recovery_skips_content_duplicate() {
        let (_t, paths) = paths_with_tmp();
        seed_recovery(
            &paths,
            "20260601T000000000-p1",
            "openai_base_url = \"X\"\n",
            Some("{\"k\":\"A\"}"),
        );

        let stale = paths.active_snapshots_dir.join("dup-session");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(config_path(&stale), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&stale), "{\"k\":\"A\"}").unwrap();
        write_manifest_to_dir(&stale, &mk_manifest("dup-session")).unwrap();

        move_snapshot_dir_to_recovery(&paths, &stale).unwrap();

        assert!(!stale.exists(), "重复内容的 stale active 应被丢弃");
        let recs = snapshot_dirs_under(&paths.recovery_snapshots_dir);
        assert_eq!(recs.len(), 1, "内容重复不应新增 recovery: {recs:?}");
    }

    /// 内容不同(哪怕只差一个字段)→ 视为新备份,保留。
    #[test]
    fn move_to_recovery_keeps_distinct_content() {
        let (_t, paths) = paths_with_tmp();
        seed_recovery(
            &paths,
            "20260601T000000000-p1",
            "openai_base_url = \"X\"\n",
            None,
        );

        let stale = paths.active_snapshots_dir.join("diff-session");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(config_path(&stale), "openai_base_url = \"Y\"\n").unwrap();
        write_manifest_to_dir(&stale, &mk_manifest("diff-session")).unwrap();

        move_snapshot_dir_to_recovery(&paths, &stale).unwrap();

        assert!(!stale.exists());
        let recs = snapshot_dirs_under(&paths.recovery_snapshots_dir);
        assert_eq!(recs.len(), 2, "不同内容应新增 recovery: {recs:?}");
    }

    /// MOC-148 review P2:config/auth **完全相同**但 manifest 的 atom `pre_value` 不同 →
    /// restore 结果不同、不可互换 → **不去重**,保留两份(否则删"重复"会丢只存在于
    /// manifest 的 atom 恢复值)。
    #[test]
    fn move_to_recovery_keeps_when_atom_pre_value_differs() {
        let (_t, paths) = paths_with_tmp();
        let existing = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(config_path(&existing), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&existing), "{\"k\":\"A\"}").unwrap();
        let mut m_existing = mk_manifest("existing");
        m_existing.electron_status_section_pre_value = Some(false);
        write_manifest_to_dir(&existing, &m_existing).unwrap();

        // config/auth 一字不差,仅 atom pre_value 不同(Some(true) vs Some(false))。
        let stale = paths.active_snapshots_dir.join("atom-diff");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(config_path(&stale), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&stale), "{\"k\":\"A\"}").unwrap();
        let mut m_stale = mk_manifest("atom-diff");
        m_stale.electron_status_section_pre_value = Some(true);
        write_manifest_to_dir(&stale, &m_stale).unwrap();

        move_snapshot_dir_to_recovery(&paths, &stale).unwrap();

        assert!(!stale.exists(), "stale 仍应移入 recovery(rename)");
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            2,
            "atom restore 值不同 → 不可互换 → 不去重,应保留两份"
        );
    }

    /// `capture_failed` 差异同样使两份不可互换(一个 restore 不动 atom、一个会复原)→ 保留。
    #[test]
    fn move_to_recovery_keeps_when_capture_failed_differs() {
        let (_t, paths) = paths_with_tmp();
        let existing = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(config_path(&existing), "openai_base_url = \"X\"\n").unwrap();
        let mut m_existing = mk_manifest("existing");
        m_existing.electron_status_section_pre_value = Some(true); // Restore(Some(true))
        write_manifest_to_dir(&existing, &m_existing).unwrap();

        let stale = paths.active_snapshots_dir.join("cap-failed");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(config_path(&stale), "openai_base_url = \"X\"\n").unwrap();
        let mut m_stale = mk_manifest("cap-failed");
        m_stale.electron_status_section_capture_failed = true; // Untouched
        write_manifest_to_dir(&stale, &m_stale).unwrap();

        move_snapshot_dir_to_recovery(&paths, &stale).unwrap();

        assert!(!stale.exists());
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            2,
            "capture_failed 不同(不动 atom vs 复原)→ 不可互换 → 保留两份"
        );
    }

    /// 回归:config/auth 相同且 atom 三态也相同 → 仍是真重复 → 照旧去重(本修复不误伤)。
    #[test]
    fn move_to_recovery_still_dedups_when_atom_state_matches() {
        let (_t, paths) = paths_with_tmp();
        let existing = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(config_path(&existing), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&existing), "{\"k\":\"A\"}").unwrap();
        let mut m_existing = mk_manifest("existing");
        m_existing.electron_status_section_pre_value = Some(true);
        write_manifest_to_dir(&existing, &m_existing).unwrap();

        let stale = paths.active_snapshots_dir.join("same-atom");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(config_path(&stale), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&stale), "{\"k\":\"A\"}").unwrap();
        let mut m_stale = mk_manifest("same-atom");
        m_stale.electron_status_section_pre_value = Some(true);
        write_manifest_to_dir(&stale, &m_stale).unwrap();

        move_snapshot_dir_to_recovery(&paths, &stale).unwrap();

        assert!(!stale.exists());
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            1,
            "config/auth + atom 三态全同 → 真重复 → 去重(不新增)"
        );
    }

    /// 两份都"真"没有 config/auth(NotFound,合法空原始态)→ 内容相同 → 视为重复。
    /// (区别于"文件存在但读失败"——那种 `snapshot_content_for_dedup` 返 None,
    /// `recovery_has_content_duplicate` 保守判非重复、保留,见 IMPORTANT#2 修复。)
    #[test]
    fn move_to_recovery_treats_both_genuinely_empty_as_duplicate() {
        let (_t, paths) = paths_with_tmp();
        let existing = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&existing).unwrap();
        write_manifest_to_dir(&existing, &mk_manifest("existing")).unwrap();

        let stale = paths.active_snapshots_dir.join("empty-session");
        std::fs::create_dir_all(&stale).unwrap();
        write_manifest_to_dir(&stale, &mk_manifest("empty-session")).unwrap();

        move_snapshot_dir_to_recovery(&paths, &stale).unwrap();

        assert!(!stale.exists(), "两份都(真)空 → 视为重复,stale 丢弃");
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            1,
            "空内容重复不应新增 recovery"
        );
    }

    /// 上限:超过 MAX_RECOVERY_SNAPSHOTS 时只保留最新 N 份(按时间戳目录名)。
    #[test]
    fn prune_recovery_caps_to_max_keeping_newest() {
        let (_t, paths) = paths_with_tmp();
        let total = MAX_RECOVERY_SNAPSHOTS + 2; // 7
        for i in 0..total {
            // day (i+1):20260601 .. 20260607,字典序==时间序,内容各不同
            let name = format!("2026060{}T120000000-p{i}", i + 1);
            seed_recovery(&paths, &name, &format!("v = {i}\n"), None);
        }
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            total
        );

        prune_recovery_snapshots(&paths);

        let remaining = snapshot_dirs_under(&paths.recovery_snapshots_dir);
        assert_eq!(remaining.len(), MAX_RECOVERY_SNAPSHOTS, "应封顶到 N 份");
        let names: Vec<String> = remaining.iter().filter_map(|d| dir_name(d)).collect();
        assert!(
            !names.iter().any(|n| n.starts_with("20260601T")),
            "最旧(day1)应被删: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("20260602T")),
            "次旧(day2)应被删: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.starts_with("20260607T")),
            "最新(day7)应保留: {names:?}"
        );
    }

    /// prune 在 snapshot_codex_state(每次 apply)里被触发 → 历史无上限积压会被收敛。
    #[test]
    fn snapshot_codex_state_prunes_existing_recovery_backlog() {
        let (_t, paths) = paths_with_tmp();
        for i in 0..(MAX_RECOVERY_SNAPSHOTS + 3) {
            let name = format!("2026053{}T120000000-p{i}", i); // 20260530..,内容各异
            seed_recovery(&paths, &name, &format!("v = {i}\n"), None);
        }
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"current\"\n").unwrap();

        snapshot_codex_state(&paths, "v-new", "New", true).unwrap();

        assert!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len() <= MAX_RECOVERY_SNAPSHOTS,
            "apply 时应把积压 recovery 收敛到上限内"
        );
    }

    /// Devin Review BUG-004 防回归:pre-v3 (schema_version=2) 的 stale active
    /// 被升级到 recovery 时,必须**同时**标 capture_failed=true,否则后续 manual
    /// restore 时(schema 已升 v3 + 三态默认 None/false)会误判"snapshot 时 atom
    /// 不存在" → remove_atom → silently 抹掉 user 手动设的 context-usage 偏好(Codex Settings)。
    #[test]
    fn pre_v3_stale_recovery_upgrade_marks_capture_failed() {
        let (_t, paths) = paths_with_tmp();

        // 模拟旧 transfer 写的 schema_version=2 manifest(没追踪 atom 字段,但
        // 序列化时 serde 给 default 值 — pre_value=None + capture_failed=false)
        let stale_dir = paths.active_snapshots_dir.join("v2-session");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(config_path(&stale_dir), "openai_base_url = \"old\"\n").unwrap();
        let v2_manifest_json = serde_json::json!({
            "schema_version": 2,
            "snapshot_id": "v2-session",
            "session_id": "v2-session",
            "snapshot_at": "2026-05-01T01:00:00",
            "config_existed": true,
            "auth_existed": false,
            "app_version": "v-old",
            "provider_name": "Old"
            // 故意 omit electron_status_section_* — 模拟真实 v2 manifest 没这俩字段
        });
        std::fs::write(manifest_path(&stale_dir), v2_manifest_json.to_string()).unwrap();

        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"new\"\n").unwrap();

        // trigger move_stale_active_snapshots_to_recovery via snapshot_codex_state
        snapshot_codex_state(&paths, "v-new", "New", true).unwrap();

        // 找新生成的 recovery manifest,核对 schema 升 v3 + capture_failed=true
        let recovery_dirs: Vec<_> = std::fs::read_dir(&paths.recovery_snapshots_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert!(!recovery_dirs.is_empty(), "应有 recovery 目录");
        let recovery_manifest =
            read_manifest_from_dir(&recovery_dirs[0]).expect("应读到 recovery manifest");
        assert_eq!(
            recovery_manifest.schema_version, SNAPSHOT_SCHEMA_VERSION,
            "schema_version 应升到 v3"
        );
        assert!(
            recovery_manifest.electron_status_section_capture_failed,
            "pre-v3 manifest 升 v3 时必须 mark capture_failed=true(防 manual restore 误删 user atom)"
        );
    }

    /// [MOC-123 / PR #360 P2] reuse 一个 legacy v3 manifest 时必须升到 v4 并重新捕获
    /// 当前 atom 原值。否则 apply 随后无条件写的 `show-context-window-usage` 没有可
    /// restore 的 pre-value(restore 跳过 `< 4` manifest)→ transfer 退出时清不掉。
    /// 这里验证 user 原本无 atom 的常见场景:升级后 pre_value=None + schema v4 + 落盘,
    /// 后续 restore 据此 remove 干净(恢复 Codex 默认)。
    #[test]
    fn reuse_upgrades_legacy_v3_manifest_and_recaptures_atom() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.snapshot_dir).unwrap();
        let v3 = serde_json::json!({
            "schema_version": 3,
            "snapshot_id": "legacy-v3",
            "session_id": "legacy-v3",
            "snapshot_at": "2026-06-01T00:00:00",
            "config_existed": false,
            "auth_existed": false,
            "app_version": "v-old",
            "provider_name": "Old",
            // v3 追踪的是旧 key,新 key 视角下没捕获;且 user 原本无新 key atom
            "electron_status_section_pre_value": null,
            "electron_status_section_capture_failed": false
        });
        std::fs::write(manifest_path(&paths.snapshot_dir), v3.to_string()).unwrap();

        let upgraded = reuse_manifest_upgrading_atom(&paths, &paths.snapshot_dir, true).unwrap();
        assert_eq!(
            upgraded.schema_version, SNAPSHOT_SCHEMA_VERSION,
            "legacy v3 manifest 复用时必须升到当前 schema(v4),否则写入的 atom 退出清不掉"
        );
        assert_eq!(
            upgraded.electron_status_section_pre_value, None,
            "user 原本无 atom → 重新捕获到 None(restore 会 remove,退出恢复 Codex 默认)"
        );
        assert!(!upgraded.electron_status_section_capture_failed);
        // 落盘也已升级到 v4
        let persisted = read_manifest_from_dir(&paths.snapshot_dir).unwrap();
        assert_eq!(persisted.schema_version, SNAPSHOT_SCHEMA_VERSION);
    }

    /// [MOC-123 / PR #360 P2] direct 直连模式(manage_atom=false):snapshot 不把 atom
    /// 当 transfer 管理字段 —— 标 capture_failed=true 让 restore 跳过,这样 direct 会话
    /// 期间 user 在 Codex Settings 手动改的圆环不会被退出 restore 覆盖(#317 留用户原值)。
    #[test]
    fn direct_snapshot_marks_atom_unmanaged_so_restore_skips() {
        let (_t, paths) = paths_with_tmp();
        // user 自己开了圆环
        crate::electron_state::write_atom(
            &paths.electron_global_state,
            crate::electron_state::CONTEXT_USAGE_ATOM_KEY,
            serde_json::json!(true),
        )
        .unwrap();
        // direct apply 的 snapshot:manage_atom=false
        let m = snapshot_codex_state(&paths, "v", "Direct", false).unwrap();
        assert!(
            m.electron_status_section_capture_failed,
            "direct 模式 snapshot 必须标 capture_failed=true,让 restore 跳过 atom(留用户原值)"
        );
        assert_eq!(m.electron_status_section_pre_value, None);
    }

    #[test]
    fn recovery_snapshot_ids_follow_unique_target_dirs() {
        let (_t, paths) = paths_with_tmp();
        let first_dir = paths.active_snapshots_dir.join("first-session");
        let second_dir = paths.active_snapshots_dir.join("second-session");

        for (dir, config) in [
            (&first_dir, "openai_base_url = \"first\"\n"),
            (&second_dir, "openai_base_url = \"second\"\n"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(config_path(dir), config).unwrap();
            std::fs::write(
                manifest_path(dir),
                serde_json::to_string(&SnapshotManifest {
                    schema_version: SNAPSHOT_SCHEMA_VERSION,
                    snapshot_id: "old-session".to_owned(),
                    session_id: dir_name(dir).unwrap(),
                    snapshot_at: "2026-05-15T01:00:00".to_owned(),
                    config_existed: true,
                    auth_existed: false,
                    app_version: "v-old".to_owned(),
                    provider_name: Some("Old".to_owned()),
                    electron_status_section_pre_value: None,
                    electron_status_section_capture_failed: false,
                })
                .unwrap(),
            )
            .unwrap();
        }

        move_snapshot_dir_to_recovery(&paths, &first_dir).unwrap();
        move_snapshot_dir_to_recovery(&paths, &second_dir).unwrap();

        let snapshots = list_snapshots(&paths);
        assert!(snapshots
            .iter()
            .any(|s| s.kind == "recovery" && s.id == "old-session"));
        assert!(snapshots
            .iter()
            .any(|s| s.kind == "recovery" && s.id == "old-session-2"));
        assert_eq!(
            read_snapshot_config_by_id(&paths, "old-session-2").unwrap(),
            "openai_base_url = \"second\"\n"
        );
    }

    #[test]
    fn snapshot_toml_value_literal_extracts() {
        let s = "# c\nopenai_base_url = \"http://x\"\nfoo = 1\n";
        assert_eq!(
            snapshot_toml_value_literal(s, "openai_base_url"),
            Some("\"http://x\"".to_owned())
        );
        assert_eq!(snapshot_toml_value_literal(s, "foo"), Some("1".to_owned()));
        assert_eq!(snapshot_toml_value_literal(s, "missing"), None);
    }

    #[test]
    fn snapshot_toml_value_literal_strips_inline_comment() {
        let s = "openai_base_url = \"http://x\" # comment\n";
        assert_eq!(
            snapshot_toml_value_literal(s, "openai_base_url"),
            Some("\"http://x\"".to_owned())
        );
    }
}
