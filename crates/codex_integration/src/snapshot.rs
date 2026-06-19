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
//! 5. **写入端反投毒**([MOC-197]):拍快照时对 `config.toml` 副本 strip
//!    transfer 签名字段(#270 `signature_fields_to_strip`)。上一 session 被
//!    强杀、退出 restore 没跑时 live config 仍带 `openai_base_url`/`sandbox_mode`
//!    等残留;若原样拍照会把污染固化成"用户原始配置"——写入端 strip 在拍照
//!    时同步清除(live config 本身不动,apply 接下来就会重写它)。
//! 6. **stale 快照自愈**([MOC-197]):被 SIGKILL/崩溃强杀的 session 遗留的
//!    active 快照,新进程通过 [`has_stale_active_snapshot`] /
//!    [`stale_active_snapshot_dirs`] 感知;退出 restore 与 `desktop_clear`
//!    守门均已补 `|| has_stale_active_snapshot` 盲区,确保强杀后重启能补跑
//!    欠下的 restore。

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::paths::CodexPaths;
use crate::toml_sync::write_atomic;
use crate::CodexError;

/// Snapshot manifest 的 schema 版本号。历史上 v3/v4 用于追踪 Codex Desktop
/// context-usage atom 的 pre-value;该 feature 已移除(MOC-229),atom 字段不再写入,
/// 但版本号保持 4 不回退 —— 旧 manifest 多出的 atom 字段由 serde 忽略
/// (`SnapshotManifest` 无 `deny_unknown_fields`),无需迁移。
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

/// 在 apply 写入前对当前 Codex 配置拍快照(幂等:已有 active 快照则复用)。
pub fn snapshot_codex_state(
    paths: &CodexPaths,
    app_version: &str,
    provider_name: &str,
    // [MOC-257 review] 写入端 signature-strip 要识别的 proxy 端口(含当前 settings.proxyPort,不止历史默认
    // 18080)。否则自定义 proxyPort(如 19090)的 transfer relay 字段拍照时识别不到、被当「用户原始」固化进
    // 快照 → restore 把它再写回(MOC-162 系统性缺口的 capture 端;这里至少让快照本身不带毒)。
    proxy_ports: &[u16],
) -> Result<SnapshotManifest, CodexError> {
    move_stale_active_snapshots_to_recovery(paths)?;

    let current_dir = current_active_snapshot_dir(paths);
    if manifest_path(&current_dir).exists() {
        return read_manifest_from_dir(&current_dir);
    }
    if paths.snapshot_manifest.exists() {
        return read_manifest_from_dir(&paths.snapshot_dir);
    }
    std::fs::create_dir_all(&current_dir)?;

    let config_existed = paths.config_toml.exists();
    // [MOC-257 review] auth 写入端反投毒(对齐下面 config 的 signature-strip):合成 auth(`cas_synthetic`)
    // 是 Transfer 伪造、**绝非**用户原始。上个 session `restoreCodexOnExit=false` 残留合成 auth 时,原样拍照会
    // 把合成固化成「用户原始 auth」(active 快照语义)→ restore 还原成合成 → standalone Codex 拿假凭据撞
    // chatgpt.com。合成则快照视为**无 auth**(auth_existed=false、不拷);真账号本就在 stash,exit/startup 的
    // restore_stashed 兜底还原。非合成(真账号 / apikey)才拍。
    let auth_is_synthetic = paths.auth_json.exists()
        && std::fs::read_to_string(&paths.auth_json)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("cas_synthetic").and_then(serde_json::Value::as_bool))
            == Some(true);
    let auth_existed = paths.auth_json.exists() && !auth_is_synthetic;

    if config_existed {
        let snapshot_copy = config_path(&current_dir);
        std::fs::copy(&paths.config_toml, &snapshot_copy)?;
        // [MOC-197] 写入端反投毒:live config 含 transfer signature 字段(上一个 session
        // 被强杀、退出 restore 没跑的残留)时,原样拍照会把污染固化成"用户原始配置"
        // (active 快照的语义),restore 基线从此带毒。#270 只防"从脏快照读回",这里补
        // 写入端对称防护:对快照**副本** strip 命中字段(高精度签名 100% transfer 写的,
        // 不会误删用户手写值;live config 本身不动,apply 接下来就会重写它)。端口列表与
        // #270 restore 端同为 [18080],自定义 proxyPort 的识别缺口由 MOC-162 统一解决。
        let content = std::fs::read_to_string(&snapshot_copy)?;
        for key in crate::residual::signature_fields_to_strip(
            &content,
            &paths.model_catalog_json,
            proxy_ports,
        ) {
            crate::toml_sync::sync_root_value(&snapshot_copy, &key, None)?;
        }
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

pub(crate) fn read_manifest_from_dir(dir: &Path) -> Result<SnapshotManifest, CodexError> {
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

pub(crate) fn read_snapshot_config_from_dir(dir: &Path) -> Option<String> {
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

pub(crate) fn read_snapshot_auth_from_dir(dir: &Path) -> serde_json::Value {
    let txt = std::fs::read_to_string(auth_path(dir)).ok();
    match txt {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(&s)
            .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
        _ => serde_json::Value::Object(Default::default()),
    }
}

/// [MOC-197] 区分"文件不存在"(`Ok(None)`)与"存在但读失败"(`Err`)的快照
/// config 读取。stale heal 用:读失败时**不能**折叠成空内容去还原(那会把
/// managed key 全删 = 破坏性 clear),必须冒泡让 caller 保守中止、保留快照目录
/// (silent-failure review HIGH#1;`read_snapshot_config_from_dir` 的 `.ok()`
/// 折叠语义保留给"按 manifest existed 标记判定"的既有路径)。
pub(crate) fn read_snapshot_config_classified(dir: &Path) -> Result<Option<String>, CodexError> {
    match std::fs::read_to_string(config_path(dir)) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// [MOC-197] 同 [`read_snapshot_config_classified`],auth 版。损坏 JSON 也算
/// 读失败(`Err`)而非空对象 —— 否则 heal 会把 live 的 managed auth key 删掉
/// 而不是按快照还原。空文件沿用既有语义(空对象)。
pub(crate) fn read_snapshot_auth_classified(
    dir: &Path,
) -> Result<Option<serde_json::Value>, CodexError> {
    match std::fs::read_to_string(auth_path(dir)) {
        Ok(s) if s.trim().is_empty() => Ok(Some(serde_json::Value::Object(Default::default()))),
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
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

/// [MOC-197] 列出**非当前 session** 的 active 快照目录(被 SIGKILL/崩溃强杀的
/// session 遗留),按目录名升序(固定宽度时间戳前缀 → 字典序即时间序,旧→新)。
/// 只认带 manifest 的目录(口径同 [`snapshot_dirs_under`])。
pub(crate) fn stale_active_snapshot_dirs(paths: &CodexPaths) -> Vec<PathBuf> {
    let current = current_session_id();
    let mut dirs: Vec<PathBuf> = snapshot_dirs_under(&paths.active_snapshots_dir)
        .into_iter()
        .filter(|dir| dir_name(dir).as_deref() != Some(current))
        .collect();
    dirs.sort_by(|a, b| dir_name(a).cmp(&dir_name(b)));
    dirs
}

/// [MOC-197] 是否存在 stale session 的 active 快照。[`has_snapshot`] 是
/// session(进程)维度、看不见它们;caller(`desktop_clear` 守门 / 退出 restore
/// gate)用本函数补盲区,避免"快照明明在却报 no snapshot"。
pub fn has_stale_active_snapshot(paths: &CodexPaths) -> bool {
    !stale_active_snapshot_dirs(paths).is_empty()
}

pub(crate) fn move_stale_active_snapshots_to_recovery(
    paths: &CodexPaths,
) -> Result<(), CodexError> {
    // 按目录名升序 = **旧→新**处理(见 stale_active_snapshot_dirs)。
    // 替换式去重(见 move_snapshot_dir_to_recovery)下,同内容多份 stale 时让最新那份**最后**
    // 处理、替换掉更旧的,使留存的 recovery 副本始终是最新那份(MOC-148 review P2:否则若
    // newer 先处理、older 后处理,older 会覆盖 newer,at-cap 时该内容可能被 prune 误删)。
    for dir in stale_active_snapshot_dirs(paths) {
        // 去重为**替换式**(见 move_snapshot_dir_to_recovery):命中旧重复时把更新的 stale
        // 作为最新一份移入,内容以最新时间戳存活 → 末尾 prune 不论 cap 状态都不会误删它。
        // 故无需"去重前先 prune"(MOC-148 review P2:那只防已超额、防不住本轮移入后才超额)。
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

/// manifest.json 的读取分类:区分"合法空态(不存在)"与"损坏(存在但读/解析失败)"。
enum ManifestRead {
    /// 文件不存在(合法:无 manifest 的空原始态)。
    Absent,
    /// 读取 + 解析成功。
    Parsed,
    /// 文件存在但读失败 / 解析失败(损坏,内容不可判定)。
    Corrupt,
}

fn read_manifest_classified(dir: &Path) -> ManifestRead {
    match std::fs::read_to_string(manifest_path(dir)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ManifestRead::Absent,
        Err(_) => ManifestRead::Corrupt,
        Ok(s) => match serde_json::from_str::<SnapshotManifest>(&s) {
            Ok(_) => ManifestRead::Parsed,
            Err(_) => ManifestRead::Corrupt,
        },
    }
}

/// 读快照 dir 的去重指纹:config.toml + auth.json 内容。
/// **不含 manifest 的 timestamp/session_id/snapshot_id**(每份都不同)。
/// config/auth 任一"存在但读失败",或 manifest **存在但损坏** → 返回 `None`
/// (内容不可判定,调用方据此保守不去重;manifest 损坏沿用 MOC-148 review P2 语义,
/// 不拿"损坏、`list_snapshots` 看不到、无法手动恢复"的份当去重目标而误删好快照)。
fn snapshot_content_for_dedup(dir: &Path) -> Option<(Option<String>, Option<String>)> {
    let config = read_dedup_field(&config_path(dir))?;
    let auth = read_dedup_field(&auth_path(dir))?;
    if matches!(read_manifest_classified(dir), ManifestRead::Corrupt) {
        return None;
    }
    Some((config, auth))
}

/// recovery/ 中与 `dir` 内容(config + auth)完全相同的备份目录列表。
///
/// 保守语义:`dir` 自身 config/auth 读不出、或 manifest **存在但损坏** → 视为内容不可判定,
/// 返回空(当作非重复、保留),绝不因"读不出/损坏"去删唯一副本;某份 recovery config/auth
/// 读不出或 manifest 损坏 → 该份不参与匹配(不拿无法手动恢复的损坏份当去重目标)。
fn recovery_content_duplicate_dirs(paths: &CodexPaths, dir: &Path) -> Vec<PathBuf> {
    let Some(target) = snapshot_content_for_dedup(dir) else {
        return Vec::new();
    };
    snapshot_dirs_under(&paths.recovery_snapshots_dir)
        .into_iter()
        .filter(|rec| snapshot_content_for_dedup(rec).as_ref() == Some(&target))
        .collect()
}

/// recovery/ 只保留最近 [`MAX_RECOVERY_SNAPSHOTS`] 份,其余物理删除。
///
/// 保留优先级:**可恢复(manifest 解析成功,即 `list_snapshots` 能看到的份)优先于损坏/
/// 不可恢复**;同组内再按目录名(固定宽度时间戳前缀 `20260603T210740197-pNNNN` → 字典序即
/// 时间序)新→旧。保留前 N 份、其余删除。即超额时**先淘汰损坏份**,不让较新的损坏快照挤掉
/// 较旧但有效的备份(MOC-148 review P2:`snapshot_dirs_under` 只看 manifest 文件存在,
/// 而 `list_snapshots` 会跳过解析失败的目录,二者口径不一致会导致只剩不可恢复的份)。
///
/// **best-effort**(MOC-148 review IMPORTANT#1):这是纯 GC,单个目录删失败
/// (并发进程占用 / 权限 / 半删残留)只 warn 跳过,**绝不冒泡**——否则会让
/// 调用链顶端的 `apply_provider` 整体失败(快照本身已成功,失败的只是清旧)。
/// 与同文件 `gc_trash_older_than` 既有约定一致。
fn prune_recovery_snapshots(paths: &CodexPaths) {
    let dirs = snapshot_dirs_under(&paths.recovery_snapshots_dir);
    if dirs.len() <= MAX_RECOVERY_SNAPSHOTS {
        return;
    }
    // 每份算一次"是否可恢复"(manifest 解析成功 = `list_snapshots` 口径),避免排序中重复 IO。
    let mut ranked: Vec<(bool, PathBuf)> = dirs
        .into_iter()
        .map(|d| (read_manifest_from_dir(&d).is_ok(), d))
        .collect();
    // 可恢复(true)优先于损坏(false);同组内按目录名(时间戳)新→旧。
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| dir_name(&b.1).cmp(&dir_name(&a.1)))
    });
    for (_, dir) in ranked.into_iter().skip(MAX_RECOVERY_SNAPSHOTS) {
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
    // 备份去重(**替换式**,MOC-148 review P2):recovery/ 已有内容(config+auth)
    // 相同的旧份时,**删掉旧份、把这份更新的 stale 作为最新一份移入**(下方 rename),
    // 而非"删 stale 保留旧份"。后者在 recovery 接近/达到上限时有内容丢失风险:旧份可能
    // 随后被 `prune_recovery_snapshots` 清掉(本轮还有别的 stale 移入顶过上限时),导致该
    // 内容在 recovery 里一份不剩。替换后内容以最新时间戳存活,prune 不论 cap 状态都不会
    // 误删;且始终只保留一份,不累积重复(用户要求:备份时字段比对,不留重复)。
    //
    // best-effort:删旧份失败只 warn(回退为"临时多留一份重复",末尾 prune 再收敛),
    // 不冒泡阻断 apply。
    for old in recovery_content_duplicate_dirs(paths, dir) {
        if let Err(e) = std::fs::remove_dir_all(&old) {
            tracing::warn!(
                target: "codex_integration::snapshot",
                dir = %old.display(),
                error = %e,
                "remove stale recovery duplicate failed; skipping (best-effort)",
            );
        }
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
        });
    std::fs::create_dir_all(&paths.recovery_snapshots_dir)?;
    let target = unique_recovery_dir(paths, &manifest.snapshot_id);
    std::fs::rename(dir, &target)?;
    // 归档进 recovery 时把 schema_version 升到当前版本(纯 metadata,无字段迁移)。
    let mut recovery_manifest = manifest;
    recovery_manifest.schema_version = SNAPSHOT_SCHEMA_VERSION;
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
        snapshot_codex_state(&paths, "v-test", "Mock", &[18080]).unwrap();
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
        let m = snapshot_codex_state(&paths, "v2.0.0-stage2.5", "Mock", &[18080]).unwrap();
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
        let m = snapshot_codex_state(&paths, "v", "Mock", &[18080]).unwrap();
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
        snapshot_codex_state(&paths, "v", "Mock", &[18080]).unwrap();
        // 改了 config.toml,再 snapshot 一次 —— 不应覆盖原始备份
        std::fs::write(&paths.config_toml, "new\n").unwrap();
        snapshot_codex_state(&paths, "v", "Mock", &[18080]).unwrap();
        assert_eq!(
            read_snapshot_config(&paths).unwrap(),
            "old\n",
            "首次快照后再次调用必须保留原始备份"
        );
    }

    #[test]
    fn drop_snapshot_clears_dir() {
        let (_t, paths) = paths_with_tmp();
        snapshot_codex_state(&paths, "v", "Mock", &[18080]).unwrap();
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
            })
            .unwrap(),
        )
        .unwrap();

        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"current\"\n").unwrap();
        snapshot_codex_state(&paths, "v-new", "New", &[18080]).unwrap();

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

    /// 备份去重(替换式):recovery/ 已有内容相同的旧份时,删旧份、把更新的 stale 作为
    /// 最新一份移入 —— 最终仍只一份(不累积重复),且内容以最新时间戳存活。
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

    /// 回归:config/auth 完全相同 → 真重复 → 去重(只保留一份)。
    #[test]
    fn move_to_recovery_dedups_when_content_matches() {
        let (_t, paths) = paths_with_tmp();
        let existing = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(config_path(&existing), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&existing), "{\"k\":\"A\"}").unwrap();
        write_manifest_to_dir(&existing, &mk_manifest("existing")).unwrap();

        let stale = paths.active_snapshots_dir.join("same-content");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(config_path(&stale), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&stale), "{\"k\":\"A\"}").unwrap();
        write_manifest_to_dir(&stale, &mk_manifest("same-content")).unwrap();

        move_snapshot_dir_to_recovery(&paths, &stale).unwrap();

        assert!(!stale.exists());
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            1,
            "config/auth 完全相同 → 真重复 → 去重(不新增)"
        );
    }

    /// MOC-148 review P2(#2):recovery 超额(已有 MAX 份**更新**的 + 1 份与 stale 同内容的
    /// **更旧**份)时,迁移不能"因旧重复存在就丢弃 stale,随后 prune 又删掉那份旧重复"——
    /// 否则该内容在 recovery 里一份不剩。替换式去重(删旧份 + 移入更新的 stale)后内容存活。
    #[test]
    fn stale_content_survives_when_recovery_over_cap_with_old_duplicate() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.recovery_snapshots_dir).unwrap();

        // 1 份"更旧"的、与 stale 同内容(C)的 recovery(最小时间戳 → prune 最先清)。
        let old_dup = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&old_dup).unwrap();
        std::fs::write(config_path(&old_dup), "openai_base_url = \"C\"\n").unwrap();
        write_manifest_to_dir(&old_dup, &mk_manifest("old-dup")).unwrap();

        // MAX 份"更新"的、内容互不相同的 recovery(时间戳更大,占满上限)。
        for i in 0..MAX_RECOVERY_SNAPSHOTS {
            let d = paths
                .recovery_snapshots_dir
                .join(format!("2026061{i}T000000000-p9"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(config_path(&d), format!("openai_base_url = \"N{i}\"\n")).unwrap();
            write_manifest_to_dir(&d, &mk_manifest(&format!("newer-{i}"))).unwrap();
        }

        // stale active(内容 C),走完整 apply 迁移流程(含前后两次 prune)。
        let stale = paths.active_snapshots_dir.join("20260620T000000000-p1");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(config_path(&stale), "openai_base_url = \"C\"\n").unwrap();
        write_manifest_to_dir(&stale, &mk_manifest("20260620T000000000-p1")).unwrap();

        move_stale_active_snapshots_to_recovery(&paths).unwrap();

        assert!(!stale.exists(), "stale active 应已处理(移入或去重)");
        let content_c_survives = snapshot_dirs_under(&paths.recovery_snapshots_dir)
            .iter()
            .any(|d| {
                read_snapshot_config_from_dir(d).as_deref() == Some("openai_base_url = \"C\"\n")
            });
        assert!(
            content_c_survives,
            "去重 + prune 后内容 C 仍应在 recovery 留有一份(不两头落空)"
        );
        assert!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len() <= MAX_RECOVERY_SNAPSHOTS,
            "recovery 仍受上限约束"
        );
    }

    /// MOC-148 review P2:recovery **恰好 = MAX**(pre-loop prune 无效)、最旧份与某 stale 同内容,
    /// 且本轮**另有一个新内容 stale**一起移入 → 末尾 prune 会把那份旧重复清掉。替换式去重把同
    /// 内容 stale 提为最新份,内容随之以最新时间戳存活,不被 prune 误删。
    #[test]
    fn stale_content_survives_at_cap_when_sibling_move_triggers_prune() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.recovery_snapshots_dir).unwrap();

        // recovery 恰好占满 MAX:最旧一份内容 = C(与下面 stale-C 相同),其余内容各异。
        let old_dup_c = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&old_dup_c).unwrap();
        std::fs::write(config_path(&old_dup_c), "openai_base_url = \"C\"\n").unwrap();
        write_manifest_to_dir(&old_dup_c, &mk_manifest("old-dup-c")).unwrap();
        for i in 1..MAX_RECOVERY_SNAPSHOTS {
            let d = paths
                .recovery_snapshots_dir
                .join(format!("2026060{}T000000000-p9", i + 1));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(config_path(&d), format!("openai_base_url = \"K{i}\"\n")).unwrap();
            write_manifest_to_dir(&d, &mk_manifest(&format!("keep-{i}"))).unwrap();
        }
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            MAX_RECOVERY_SNAPSHOTS,
            "前置:recovery 恰好占满 MAX(pre-loop prune 此时是 no-op)"
        );

        // 本轮两个 stale active:一个内容 C(与最旧份重复),一个新内容 NEW。
        let stale_c = paths.active_snapshots_dir.join("20260620T000000000-p1");
        std::fs::create_dir_all(&stale_c).unwrap();
        std::fs::write(config_path(&stale_c), "openai_base_url = \"C\"\n").unwrap();
        write_manifest_to_dir(&stale_c, &mk_manifest("20260620T000000000-p1")).unwrap();

        let stale_new = paths.active_snapshots_dir.join("20260621T000000000-p1");
        std::fs::create_dir_all(&stale_new).unwrap();
        std::fs::write(config_path(&stale_new), "openai_base_url = \"NEW\"\n").unwrap();
        write_manifest_to_dir(&stale_new, &mk_manifest("20260621T000000000-p1")).unwrap();

        move_stale_active_snapshots_to_recovery(&paths).unwrap();

        let content_c_survives = snapshot_dirs_under(&paths.recovery_snapshots_dir)
            .iter()
            .any(|d| {
                read_snapshot_config_from_dir(d).as_deref() == Some("openai_base_url = \"C\"\n")
            });
        assert!(
            content_c_survives,
            "at-cap + 兄弟移入触发 prune 后,内容 C 仍应存活(替换式去重提为最新份)"
        );
        assert!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len() <= MAX_RECOVERY_SNAPSHOTS,
            "recovery 仍受上限约束"
        );
    }

    /// MOC-148 review P2:active/ 有两份同内容 stale(新/旧),替换式去重必须让**最新**那份成为
    /// 留存副本(否则旧份覆盖新份,at-cap 时可能被 prune 误删)。构造 recovery 占满 MAX(时间戳
    /// 居中)+ 旧 stale + 新 stale 同内容 → 迁移后内容存活,且留存副本是新 stale 那份。
    #[test]
    fn move_to_recovery_keeps_newest_among_duplicate_stale_actives() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.recovery_snapshots_dir).unwrap();
        // recovery 占满 MAX,时间戳居中(晚于旧 stale、早于新 stale),内容各异。
        for i in 0..MAX_RECOVERY_SNAPSHOTS {
            let d = paths
                .recovery_snapshots_dir
                .join(format!("20260615T00000000{i}-p9"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(config_path(&d), format!("openai_base_url = \"M{i}\"\n")).unwrap();
            write_manifest_to_dir(&d, &mk_manifest(&format!("mid-{i}"))).unwrap();
        }
        // 两份同内容(C)stale:旧(20260610)+ 新(20260620)。
        let older_c = paths.active_snapshots_dir.join("20260610T000000000-p1");
        std::fs::create_dir_all(&older_c).unwrap();
        std::fs::write(config_path(&older_c), "openai_base_url = \"C\"\n").unwrap();
        write_manifest_to_dir(&older_c, &mk_manifest("20260610T000000000-p1")).unwrap();
        let newer_c = paths.active_snapshots_dir.join("20260620T000000000-p1");
        std::fs::create_dir_all(&newer_c).unwrap();
        std::fs::write(config_path(&newer_c), "openai_base_url = \"C\"\n").unwrap();
        write_manifest_to_dir(&newer_c, &mk_manifest("20260620T000000000-p1")).unwrap();

        move_stale_active_snapshots_to_recovery(&paths).unwrap();

        let c_dirs: Vec<_> = snapshot_dirs_under(&paths.recovery_snapshots_dir)
            .into_iter()
            .filter(|d| {
                read_snapshot_config_from_dir(d).as_deref() == Some("openai_base_url = \"C\"\n")
            })
            .collect();
        assert_eq!(c_dirs.len(), 1, "内容 C 应恰好留一份(替换式去重不累积)");
        assert!(
            dir_name(&c_dirs[0])
                .map(|n| n.starts_with("20260620"))
                .unwrap_or(false),
            "留存的 C 副本应是**最新** stale(20260620)而非旧份(20260610):实际 {:?}",
            dir_name(&c_dirs[0])
        );
    }

    /// MOC-148 review P2:recovery 目录 manifest **存在但损坏**(解析失败)、config/auth 可读时,
    /// 不能与 stale 判为重复 —— 否则删掉刚生成的好 stale,只剩损坏份(`list_snapshots` 看不到、
    /// 无法手动恢复)。损坏份视为非匹配 → 好 stale 应保留移入。
    #[test]
    fn move_to_recovery_keeps_stale_when_recovery_manifest_corrupt() {
        let (_t, paths) = paths_with_tmp();
        // recovery:config/auth 与 stale 完全相同,但 manifest.json 损坏(非法 JSON)。
        let corrupt = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&corrupt).unwrap();
        std::fs::write(config_path(&corrupt), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&corrupt), "{\"k\":\"A\"}").unwrap();
        std::fs::write(manifest_path(&corrupt), "{ not valid json").unwrap();

        let stale = paths.active_snapshots_dir.join("good-session");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(config_path(&stale), "openai_base_url = \"X\"\n").unwrap();
        std::fs::write(auth_path(&stale), "{\"k\":\"A\"}").unwrap();
        write_manifest_to_dir(&stale, &mk_manifest("good-session")).unwrap();

        move_snapshot_dir_to_recovery(&paths, &stale).unwrap();

        assert!(!stale.exists(), "stale 应被移入,而非因损坏份去重而丢弃");
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            2,
            "损坏 recovery manifest 不参与去重,好 stale 应保留移入(损坏份 + 好份)"
        );
        assert!(
            list_snapshots(&paths).iter().any(|s| s.kind == "recovery"),
            "应至少有一份 manifest 完好、可被 list_snapshots 恢复的 recovery"
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

    /// MOC-148 review P2:prune 应优先保留**可恢复**(manifest 解析成功)的份、先淘汰损坏份;
    /// 不让较新的损坏快照挤掉较旧但有效的备份。
    #[test]
    fn prune_evicts_corrupt_before_valid_recovery() {
        let (_t, paths) = paths_with_tmp();
        std::fs::create_dir_all(&paths.recovery_snapshots_dir).unwrap();
        // 1 份较旧但有效的备份。
        let valid_old = paths.recovery_snapshots_dir.join("20260601T000000000-p1");
        std::fs::create_dir_all(&valid_old).unwrap();
        std::fs::write(config_path(&valid_old), "openai_base_url = \"VALID\"\n").unwrap();
        write_manifest_to_dir(&valid_old, &mk_manifest("valid-old")).unwrap();
        // MAX 份较新但 manifest 损坏的份(总数 MAX+1 → 触发 prune)。
        for i in 0..MAX_RECOVERY_SNAPSHOTS {
            let d = paths
                .recovery_snapshots_dir
                .join(format!("2026069{i}T000000000-p9"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(config_path(&d), format!("openai_base_url = \"X{i}\"\n")).unwrap();
            std::fs::write(manifest_path(&d), "{ corrupt").unwrap();
        }

        prune_recovery_snapshots(&paths);

        assert!(valid_old.exists(), "可恢复的旧备份不应被较新的损坏份挤掉");
        assert_eq!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len(),
            MAX_RECOVERY_SNAPSHOTS,
            "prune 后应恰好 MAX 份"
        );
        assert!(
            list_snapshots(&paths).iter().any(|s| s.kind == "recovery"),
            "至少保留一份可被 list_snapshots 恢复的 recovery"
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

        snapshot_codex_state(&paths, "v-new", "New", &[18080]).unwrap();

        assert!(
            snapshot_dirs_under(&paths.recovery_snapshots_dir).len() <= MAX_RECOVERY_SNAPSHOTS,
            "apply 时应把积压 recovery 收敛到上限内"
        );
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
