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
}

impl LogBuffer {
    pub fn new(max_size: usize) -> Self {
        Self {
            logs: Mutex::new(Vec::new()),
            max_size,
            file_lock: Mutex::new(()),
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
        let Some(dir) = proxy_log_dir() else {
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
        let Some(dir) = proxy_log_dir() else {
            return;
        };
        if !dir.is_dir() {
            return;
        }
        let backup_dir = proxy_log_backup_dir();
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

static TELEMETRY: OnceLock<ProxyTelemetry> = OnceLock::new();

pub fn proxy_telemetry() -> &'static ProxyTelemetry {
    TELEMETRY.get_or_init(ProxyTelemetry::default)
}

pub fn proxy_log_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("logs"))
}

fn proxy_log_backup_dir() -> PathBuf {
    proxy_log_dir()
        .unwrap_or_else(|| PathBuf::from(".codex-app-transfer").join("logs"))
        .join("backup")
}
