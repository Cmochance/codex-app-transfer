use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Local;
use codex_app_transfer_registry::config_dir;
use serde_json::{json, Value};

const MAX_STORED_BUNDLES: usize = 50;
const MAX_STORED_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct UpstreamErrorBundleInput {
    pub method: String,
    pub client_path: String,
    pub upstream_url: String,
    pub status_code: u16,
    pub provider_id: String,
    pub provider_name: String,
    pub original_model: Option<String>,
    pub resolved_model: Option<String>,
    pub upstream_model: Option<String>,
    pub outbound_headers_redacted: String,
    pub request_body: Vec<u8>,
    pub response_body: Vec<u8>,
    // MOC-32 diagnostic build: 全抓字段(write_upstream_error_bundle 跳过这些,
    // 只 write_proxy_trace_jsonl 用)。用 Default 让旧 caller 兼容。
    pub client_query: Option<String>,
    pub inbound_headers_full: serde_json::Value,
    pub inbound_body_raw: Vec<u8>,
    pub outbound_headers_full: serde_json::Value,
    pub response_headers: serde_json::Value,
    pub transfer_log_entries: serde_json::Value,
}

/// HeaderMap → JSON object {name: value}。non-utf8 value 标记 `<non-utf8 len=N>`。
/// **不 redact** — MOC-32 user 显式要全抓,本地 log only。
pub fn headers_to_json(h: &reqwest::header::HeaderMap) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (name, value) in h.iter() {
        let v = match value.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => format!("<non-utf8 len={}>", value.as_bytes().len()),
        };
        out.entry(name.as_str().to_string())
            .and_modify(|prev| {
                // duplicate header: 拼成 array
                if let serde_json::Value::Array(arr) = prev {
                    arr.push(serde_json::Value::String(v.clone()));
                } else {
                    let prev_v = prev.clone();
                    *prev = serde_json::Value::Array(vec![
                        prev_v,
                        serde_json::Value::String(v.clone()),
                    ]);
                }
            })
            .or_insert(serde_json::Value::String(v));
    }
    serde_json::Value::Object(out)
}

/// Proxy telemetry log buffer snapshot → JSON array。
pub fn log_entries_to_json(entries: &[crate::telemetry::ProxyLogEntry]) -> serde_json::Value {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        out.push(json!({
            "time": e.time,
            "level": e.level,
            "message": e.message,
        }));
    }
    serde_json::Value::Array(out)
}

pub fn feedback_bundle_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("feedback-bundles"))
}

/// MOC-32 diagnostic build: always-on proxy request/response trace → jsonl.
///
/// 写到 `~/Library/Logs/codex-app-transfer/proxy-trace-<UTC>.jsonl`(macOS),
/// append-only,一条 trace = 一行 JSON。**所有** proxy 转发都写(不只 error),
/// user 可用 `jq` 离线分析 Codex ↔ LLM 全流量。
///
/// `response_body` 在 streaming success 场景传 `&Bytes::new()`(无法预 buffer);
/// error 路径传 captured body。文件按进程生命周期 append,启动时 UTC 时间命名。
pub fn write_proxy_trace_jsonl(input: &UpstreamErrorBundleInput) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home)
        .join("Library")
        .join("Logs")
        .join("codex-app-transfer");
    fs::create_dir_all(&dir).ok()?;
    static TRACE_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let path = TRACE_PATH.get_or_init(|| {
        let ts = Local::now().format("proxy-trace-%Y%m%dT%H%M%S.jsonl");
        dir.join(ts.to_string())
    });
    let now = Local::now();
    let entry = json!({
        "kind": "proxy_request_trace",
        "captured_at": now.to_rfc3339(),
        "proxy_version": env!("CARGO_PKG_VERSION"),
        // ── inbound:Codex → transfer ──
        "inbound": {
            "method": input.method,
            "client_path": input.client_path,
            "client_query": input.client_query,
            "headers_full": input.inbound_headers_full,
            "body_raw": bytes_payload(&input.inbound_body_raw, usize::MAX),
        },
        // ── transfer 内部 adapter / 路由处理后,转发到上游的 ──
        "outbound": {
            "upstream_url": input.upstream_url,
            "headers_full": input.outbound_headers_full,
            "headers_redacted_legacy": input.outbound_headers_redacted,
            "body_transformed": bytes_payload(&input.request_body, usize::MAX),
            "provider": {
                "id": input.provider_id,
                "name": input.provider_name,
            },
            "models": {
                "original": input.original_model,
                "resolved": input.resolved_model,
                "upstream": input.upstream_model,
            },
        },
        // ── 上游 response ──
        "response": {
            "status_code": input.status_code,
            "headers": input.response_headers,
            "body": bytes_payload(&input.response_body, usize::MAX),
        },
        // ── transfer 内部 telemetry log buffer 快照(转换/路由/retry 过程) ──
        "transfer_log_entries": input.transfer_log_entries,
    });
    let mut line = serde_json::to_vec(&entry).ok()?;
    line.push(b'\n');
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.as_path())
        .ok()?;
    f.write_all(&line).ok()?;
    Some(path.clone())
}

pub fn write_upstream_error_bundle(input: &UpstreamErrorBundleInput) -> Option<PathBuf> {
    let dir = feedback_bundle_dir()?;
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }
    trim_old_bundles(&dir, MAX_STORED_BUNDLES);
    let now = Local::now();
    let bundle = json!({
        "kind": "upstream_error_bundle",
        "captured_at": now.to_rfc3339(),
        "proxy_version": env!("CARGO_PKG_VERSION"),
        "request": {
            "method": input.method,
            "client_path": input.client_path,
            "upstream_url": input.upstream_url,
            "status_code": input.status_code,
            "provider": {
                "id": input.provider_id,
                "name": input.provider_name,
            },
            "models": {
                "original": input.original_model,
                "resolved": input.resolved_model,
                "upstream": input.upstream_model,
            },
            "outbound_headers_redacted": input.outbound_headers_redacted,
            "body": bytes_payload(&input.request_body, MAX_STORED_BODY_BYTES),
        },
        "response": {
            "body": bytes_payload(&input.response_body, MAX_STORED_BODY_BYTES),
        },
    });
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!(
        "bundle-{}-{}-{}.json",
        now.format("%Y%m%d-%H%M%S"),
        std::process::id(),
        ts
    );
    let path = dir.join(filename);
    let encoded = serde_json::to_vec_pretty(&bundle).ok()?;
    fs::write(&path, encoded).ok()?;
    Some(path)
}

pub fn recent_feedback_bundles(limit: usize) -> Vec<PathBuf> {
    let Some(dir) = feedback_bundle_dir() else {
        return Vec::new();
    };
    list_recent_json_files(&dir, limit)
}

fn list_recent_json_files(dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    for item in rd.flatten() {
        let path = item.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = item.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        entries.push((modified, path));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

fn bytes_payload(bytes: &[u8], max_bytes: usize) -> Value {
    let (slice, truncated_bytes) = if bytes.len() > max_bytes {
        (&bytes[..max_bytes], bytes.len() - max_bytes)
    } else {
        (bytes, 0usize)
    };
    match std::str::from_utf8(slice) {
        Ok(text) => json!({
            "encoding": "utf8",
            "bytes": bytes.len(),
            "truncated_bytes": truncated_bytes,
            "content": text,
        }),
        Err(_) => json!({
            "encoding": "base64",
            "bytes": bytes.len(),
            "truncated_bytes": truncated_bytes,
            "content": STANDARD.encode(slice),
        }),
    }
}

fn trim_old_bundles(dir: &Path, keep: usize) {
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for item in rd.flatten() {
        let path = item.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = item.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        files.push((modified, path));
    }
    if files.len() <= keep {
        return;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_payload_preserves_utf8_and_binary() {
        let text = bytes_payload(br#"{"ok":true}"#, 1024);
        assert_eq!(text["encoding"], "utf8");
        assert_eq!(text["content"], r#"{"ok":true}"#);

        let bin = bytes_payload(&[0xff, 0xfe, 0xfd], 1024);
        assert_eq!(bin["encoding"], "base64");
        assert!(bin["content"].as_str().unwrap_or("").len() >= 4);
    }

    #[test]
    fn bytes_payload_truncates_large_content() {
        let long = "a".repeat(20);
        let v = bytes_payload(long.as_bytes(), 8);
        assert_eq!(v["bytes"], 20);
        assert_eq!(v["truncated_bytes"], 12);
        assert_eq!(v["content"], "aaaaaaaa");
    }
}
