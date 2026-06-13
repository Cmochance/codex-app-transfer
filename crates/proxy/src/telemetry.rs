//! 代理统计与日志缓冲。
//!
//! 这是 `v1.0.3:backend/proxy.py` 中 `ProxyStats`、`LogBuffer` 和全局
//! `stats` / `log_buffer` 的 Rust 等价转译。

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use chrono::{DateTime, Local};
use codex_app_transfer_registry::config_dir;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ProxyStatsSnapshot {
    pub total: u64,
    pub success: u64,
    pub failed: u64,
    pub today: u64,
}

#[derive(Debug)]
struct ProxyStatsState {
    total: u64,
    success: u64,
    failed: u64,
    today: u64,
    date: String,
}

impl Default for ProxyStatsState {
    fn default() -> Self {
        Self {
            total: 0,
            success: 0,
            failed: 0,
            today: 0,
            date: Local::now().format("%Y-%m-%d").to_string(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ProxyStats {
    inner: Mutex<ProxyStatsState>,
}

impl ProxyStats {
    pub fn record(&self, success: bool) {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut inner = self.inner.lock().unwrap();
        inner.total += 1;
        if inner.date != today {
            inner.today = 0;
            inner.date = today;
        }
        inner.today += 1;
        if success {
            inner.success += 1;
        } else {
            inner.failed += 1;
        }
    }

    pub fn snapshot(&self) -> ProxyStatsSnapshot {
        let inner = self.inner.lock().unwrap();
        ProxyStatsSnapshot {
            total: inner.total,
            success: inner.success,
            failed: inner.failed,
            today: inner.today,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProxyLogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug)]
pub struct LogBuffer {
    logs: Mutex<Vec<ProxyLogEntry>>,
    max_size: usize,
    file_lock: Mutex<()>,
    log_dir_override: Option<PathBuf>,
}

impl LogBuffer {
    pub fn new(max_size: usize) -> Self {
        Self {
            logs: Mutex::new(Vec::new()),
            max_size,
            file_lock: Mutex::new(()),
            log_dir_override: None,
        }
    }

    #[cfg(test)]
    fn new_in_dir(max_size: usize, log_dir: PathBuf) -> Self {
        Self {
            logs: Mutex::new(Vec::new()),
            max_size,
            file_lock: Mutex::new(()),
            log_dir_override: Some(log_dir),
        }
    }

    pub fn add(&self, level: impl Into<String>, message: impl Into<String>) {
        let now = Local::now();
        let level = level.into();
        let message = message.into();
        {
            let mut logs = self.logs.lock().unwrap();
            logs.push(ProxyLogEntry {
                time: now.format("%H:%M:%S").to_string(),
                level: level.clone(),
                message: message.clone(),
            });
            if logs.len() > self.max_size {
                let keep_from = logs.len() - self.max_size;
                logs.drain(0..keep_from);
            }
        }
        self.append_to_file(now, &level, &message);
    }

    pub fn get_all(&self) -> Vec<ProxyLogEntry> {
        self.logs.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.logs.lock().unwrap().clear();
        self.archive_logs();
    }

    fn append_to_file(&self, now: DateTime<Local>, level: &str, message: &str) {
        let Some(dir) = self.log_dir() else {
            return;
        };
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join(format!("proxy-{}.log", now.format("%Y-%m-%d")));
        let _guard = self.file_lock.lock().unwrap();
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let _ = writeln!(
            file,
            "{}\t{}\t{}",
            now.format("%Y-%m-%d %H:%M:%S"),
            level,
            message
        );
    }

    fn archive_logs(&self) {
        let Some(dir) = self.log_dir() else {
            return;
        };
        if !dir.is_dir() {
            return;
        }
        let backup_dir = self.log_backup_dir();
        if fs::create_dir_all(&backup_dir).is_err() {
            return;
        }
        let tag = Local::now().format("%Y%m%d-%H%M%S").to_string();
        let _guard = self.file_lock.lock().unwrap();
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let src = entry.path();
            let Some(name) = src.file_name().and_then(|v| v.to_str()) else {
                continue;
            };
            if !name.starts_with("proxy-") || !name.ends_with(".log") || !src.is_file() {
                continue;
            }
            let base = name.trim_end_matches(".log");
            let mut dst = backup_dir.join(format!("{base}_{tag}.log"));
            let mut counter = 1;
            while dst.exists() {
                dst = backup_dir.join(format!("{base}_{tag}_{counter}.log"));
                counter += 1;
            }
            let _ = fs::rename(&src, dst);
        }
    }

    fn log_dir(&self) -> Option<PathBuf> {
        self.log_dir_override.clone().or_else(proxy_log_dir)
    }

    fn log_backup_dir(&self) -> PathBuf {
        self.log_dir()
            .unwrap_or_else(|| PathBuf::from(".codex-app-transfer").join("logs"))
            .join("backup")
    }
}

#[derive(Debug)]
pub struct ProxyTelemetry {
    pub stats: ProxyStats,
    pub logs: LogBuffer,
}

impl Default for ProxyTelemetry {
    fn default() -> Self {
        Self {
            stats: ProxyStats::default(),
            logs: LogBuffer::new(200),
        }
    }
}

/// [MOC-231] 上下文 by-source 明细的**按对话持久 store**(磁盘):
/// `~/.codex-app-transfer/context-breakdown/<conversation_id>.json`。
///
/// `conversation_id` = Codex 请求的 `prompt_cache_key`(== rollout 文件名 uuid ==
/// renderer fiber 的 conversationId,2026-06-14 实证三者一致)。proxy 在 forward 完成时
/// 按该 uuid 写盘(producer),quota injector daemon 按**活动会话 uuid**(fiber 回读)读盘
/// (consumer)。磁盘持久 → transfer 重启即用、不需新对话;小 JSON → 读取快;按 uuid 隔离
/// → 切对话不串(对齐 MOC-230 累计/缓存的对话隔离)。
fn context_breakdown_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("context-breakdown"))
}

/// 校验 conversation_id 是规范 uuid(防路径穿越:只允许 hex + 连字符、长度 36)。
fn is_safe_conversation_id(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// 按对话 uuid 持久化明细(best-effort:失败不影响主转发路径)。
pub fn persist_context_breakdown(conversation_id: &str, breakdown: &serde_json::Value) {
    if !is_safe_conversation_id(conversation_id) {
        return;
    }
    let Some(dir) = context_breakdown_dir() else {
        return;
    };
    // best-effort:失败不影响转发主路径,但记 debug 便于区分「该对话还没经过 proxy」(正常,
    // 不显面板)与「写盘一直失败」(权限/磁盘满 → 面板永不显 → 否则无从诊断)。
    if let Err(e) = fs::create_dir_all(&dir) {
        tracing::debug!(error = %e, "context_breakdown 持久化建目录失败");
        return;
    }
    let path = dir.join(format!("{conversation_id}.json"));
    match serde_json::to_vec(breakdown) {
        Ok(bytes) => {
            if let Err(e) = fs::write(&path, bytes) {
                tracing::debug!(error = %e, path = %path.display(), "context_breakdown 持久化写盘失败");
            }
        }
        Err(e) => tracing::debug!(error = %e, "context_breakdown 序列化失败"),
    }
}

/// 按对话 uuid 读最近持久化的明细(quota injector daemon 每 tick 按活动会话读)。
pub fn load_context_breakdown(conversation_id: &str) -> Option<serde_json::Value> {
    if !is_safe_conversation_id(conversation_id) {
        return None;
    }
    let path = context_breakdown_dir()?.join(format!("{conversation_id}.json"));
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// [MOC-231] GC `context-breakdown/` 下 mtime 超 `max_age` 的明细文件。每对话一个小 JSON、
/// 无上限会随历史对话数长期累积;陈旧对话的明细本就过时,删了下次有请求会重建。best-effort,
/// 启动时跑一次(对齐 sessions/trash 的 retention 思路)。
pub fn gc_context_breakdown(max_age: std::time::Duration) {
    let Some(dir) = context_breakdown_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return; // 目录还不存在 = 没持久化过,正常
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mt| now.duration_since(mt).ok())
            .is_some_and(|age| age > max_age);
        if too_old {
            let _ = fs::remove_file(&path);
        }
    }
}

static TELEMETRY: OnceLock<ProxyTelemetry> = OnceLock::new();

pub fn proxy_telemetry() -> &'static ProxyTelemetry {
    TELEMETRY.get_or_init(ProxyTelemetry::default)
}

pub fn proxy_log_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakdown_conversation_id_rejects_path_traversal() {
        // [MOC-231] 持久化文件名直接来自 conversation_id(prompt_cache_key),必须严格校验
        // 是规范 uuid,否则恶意/畸形 id 能写到任意路径。合法 uuid 通过:
        assert!(is_safe_conversation_id(
            "019ec12f-eef0-7971-9bc8-ee9f0c21b5df"
        ));
        // 路径穿越 / 斜杠 / 非 hex / 错长度 / 带后缀 一律拒绝:
        assert!(!is_safe_conversation_id("../../etc/passwd"));
        assert!(!is_safe_conversation_id(
            "019ec12f/eef0-7971-9bc8-ee9f0c21b5df"
        ));
        assert!(!is_safe_conversation_id(".."));
        assert!(!is_safe_conversation_id(""));
        assert!(!is_safe_conversation_id(
            "019ec12f-eef0-7971-9bc8-ee9f0c21b5df.json"
        ));
        assert!(!is_safe_conversation_id(
            "z19ec12f-eef0-7971-9bc8-ee9f0c21b5dz"
        ));
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("codex-app-transfer-{name}-{nanos}"))
    }

    #[test]
    fn stats_records_success_failed_and_today() {
        let stats = ProxyStats::default();

        stats.record(true);
        stats.record(false);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.total, 2);
        assert_eq!(snapshot.success, 1);
        assert_eq!(snapshot.failed, 1);
        assert_eq!(snapshot.today, 2);
    }

    #[test]
    fn log_buffer_keeps_recent_entries_and_writes_daily_file() {
        let dir = unique_temp_dir("logs-write");
        let buffer = LogBuffer::new_in_dir(2, dir.clone());

        buffer.add("INFO", "first request");
        buffer.add("ERROR", "failed request");
        buffer.add("SUCCESS", "finished request");

        let entries = buffer.get_all();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level, "ERROR");
        assert_eq!(entries[0].message, "failed request");
        assert_eq!(entries[1].level, "SUCCESS");
        assert_eq!(entries[1].message, "finished request");

        let today = Local::now().format("%Y-%m-%d").to_string();
        let log_path = dir.join(format!("proxy-{today}.log"));
        let content = fs::read_to_string(log_path).unwrap();
        assert!(content.contains("\tINFO\tfirst request"));
        assert!(content.contains("\tERROR\tfailed request"));
        assert!(content.contains("\tSUCCESS\tfinished request"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn log_buffer_clear_archives_proxy_log_files() {
        let dir = unique_temp_dir("logs-clear");
        let buffer = LogBuffer::new_in_dir(20, dir.clone());

        buffer.add("INFO", "before clear");
        let today = Local::now().format("%Y-%m-%d").to_string();
        let log_path = dir.join(format!("proxy-{today}.log"));
        assert!(log_path.exists());

        buffer.clear();

        assert!(buffer.get_all().is_empty());
        assert!(!log_path.exists());

        let backup_dir = dir.join("backup");
        let archived: Vec<PathBuf> = fs::read_dir(&backup_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert_eq!(archived.len(), 1);
        assert!(archived[0]
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .starts_with(&format!("proxy-{today}_")));
        let content = fs::read_to_string(&archived[0]).unwrap();
        assert!(content.contains("\tINFO\tbefore clear"));

        let _ = fs::remove_dir_all(dir);
    }
}
