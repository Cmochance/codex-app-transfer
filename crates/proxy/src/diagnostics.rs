use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Local;
use codex_app_transfer_registry::config_dir;
use serde_json::{json, Value};

const MAX_STORED_BUNDLES: usize = 50;
const MAX_STORED_BODY_BYTES: usize = 256 * 1024;

/// 临时诊断 wire-dump:开启 `CODEX_APP_TRANSFER_WIRE_DUMP=1` 后,每次 outbound 都把
/// 完整 request body + headers + provider 元数据原样落盘到
/// `~/.codex-app-transfer/wire-dumps/YYYY-MM-DD/`,**不截断**,用于排查"上下文丢失"
/// 类 issue —— `write_upstream_error_bundle` 只在 error 路径触发且截到 256 KB,
/// success 路径完全不存,Codex CLI client 是否真的把 multi-turn history 透传给
/// proxy 无法在 success 流上验证。本机诊断用,默认关闭。
const WIRE_DUMP_ENV: &str = "CODEX_APP_TRANSFER_WIRE_DUMP";
const MAX_WIRE_DUMPS_PER_DAY: usize = 200;

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
}

pub fn feedback_bundle_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("feedback-bundles"))
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

// ═══════════════════════════════════════════════════════════════════════════
// 临时诊断:success-path wire-dump(env flag 开启)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct WireDumpInput {
    pub method: String,
    pub client_path: String,
    pub upstream_url: String,
    pub provider_id: String,
    pub provider_name: String,
    pub auth_scheme: String,
    pub original_model: Option<String>,
    pub resolved_model: Option<String>,
    pub upstream_model: Option<String>,
    pub outbound_headers_redacted: String,
    pub request_body: Vec<u8>,
}

/// 检查 env flag 是否打开。值任意非空且非 "0" / "false" / "off" 都视为开启。
pub fn wire_dump_enabled() -> bool {
    match std::env::var(WIRE_DUMP_ENV) {
        Ok(v) => parse_wire_dump_flag(&v),
        Err(_) => false,
    }
}

/// 纯函数解析 env flag 值,抽出来方便单测覆盖各种 falsy / truthy 字符串
/// (大小写、`0`/`false`/`off`/`no`、空白边距等),不依赖进程 env state
/// (避免 Rust 1.83+ `std::env::set_var` unsafe 引入的多线程测试隐患)。
pub(crate) fn parse_wire_dump_flag(value: &str) -> bool {
    let t = value.trim().to_ascii_lowercase();
    !(t.is_empty() || t == "0" || t == "false" || t == "off" || t == "no")
}

pub fn wire_dump_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("wire-dumps"))
}

/// 完整落盘一次 outbound request,body **不截断**(诊断专用,默认 env flag 关闭)。
/// 日级子目录 `wire-dumps/YYYY-MM-DD/` 单日上限 `MAX_WIRE_DUMPS_PER_DAY` 条,
/// 超额时按 mtime 删最老的。文件名 `<HHMMSS>-<pid>-<ms>.json` 便于按时间 grep。
pub fn write_wire_dump(input: &WireDumpInput) -> Option<PathBuf> {
    if !wire_dump_enabled() {
        return None;
    }
    let base = wire_dump_dir()?;
    write_wire_dump_to(&base, input)
}

/// `write_wire_dump` 的可测试核心:不读 env,目标 base 目录显式传入。
/// 单测用 `tempfile::TempDir` 提供 `base`,验证日级子目录创建 + 文件生成 +
/// body 完整不截断 + 多次调用按上限 trim 老文件 —— 不污染进程 env state,
/// 也不依赖 `~/.codex-app-transfer/`(开发机上可能已有真实数据)。
pub(crate) fn write_wire_dump_to(base: &Path, input: &WireDumpInput) -> Option<PathBuf> {
    let now = Local::now();
    let day_dir = base.join(now.format("%Y-%m-%d").to_string());
    if fs::create_dir_all(&day_dir).is_err() {
        return None;
    }
    trim_old_files(&day_dir, MAX_WIRE_DUMPS_PER_DAY);
    let bundle = json!({
        "kind": "wire_dump",
        "captured_at": now.to_rfc3339(),
        "proxy_version": env!("CARGO_PKG_VERSION"),
        "request": {
            "method": input.method,
            "client_path": input.client_path,
            "upstream_url": input.upstream_url,
            "provider": {
                "id": input.provider_id,
                "name": input.provider_name,
                "auth_scheme": input.auth_scheme,
            },
            "models": {
                "original": input.original_model,
                "resolved": input.resolved_model,
                "upstream": input.upstream_model,
            },
            "outbound_headers_redacted": input.outbound_headers_redacted,
            "body": bytes_payload_full(&input.request_body),
        },
    });
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!(
        "{}-{}-{}.json",
        now.format("%H%M%S"),
        std::process::id(),
        ts
    );
    let path = day_dir.join(filename);
    let encoded = serde_json::to_vec_pretty(&bundle).ok()?;
    fs::write(&path, encoded).ok()?;
    Some(path)
}

/// `bytes_payload` 的不截断版本,wire-dump 专用 —— 诊断时缺哪一段都可能漏掉
/// 关键证据(上下文丢失就是要看完整 contents 数组),所以不限制大小。
fn bytes_payload_full(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => json!({
            "encoding": "utf8",
            "bytes": bytes.len(),
            "truncated_bytes": 0,
            "content": text,
        }),
        Err(_) => json!({
            "encoding": "base64",
            "bytes": bytes.len(),
            "truncated_bytes": 0,
            "content": STANDARD.encode(bytes),
        }),
    }
}

fn trim_old_files(dir: &Path, keep: usize) {
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

    // ─── wire-dump 诊断 patch ───────────────────────────────────────────────

    #[test]
    fn parse_wire_dump_flag_recognizes_falsy_values() {
        for s in ["", "0", "false", "False", "OFF", "no", "  off  "] {
            assert!(
                !parse_wire_dump_flag(s),
                "expected `{s}` to parse as disabled (falsy / empty)"
            );
        }
    }

    #[test]
    fn parse_wire_dump_flag_recognizes_truthy_values() {
        for s in ["1", "true", "yes", "on", "anything", "  1  "] {
            assert!(
                parse_wire_dump_flag(s),
                "expected `{s}` to parse as enabled (any non-falsy value)"
            );
        }
    }

    fn sample_wire_dump_input(body: Vec<u8>) -> WireDumpInput {
        WireDumpInput {
            method: "POST".into(),
            client_path: "/responses".into(),
            upstream_url:
                "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse"
                    .into(),
            provider_id: "antigravity-oauth".into(),
            provider_name: "Antigravity".into(),
            auth_scheme: "GoogleOauthAntigravity".into(),
            original_model: Some("gpt-5.5".into()),
            resolved_model: Some("gemini-3.1-pro-high".into()),
            upstream_model: Some("gemini-3.1-pro-high".into()),
            outbound_headers_redacted: "content-type=application/json".into(),
            request_body: body,
        }
    }

    #[test]
    fn write_wire_dump_to_persists_full_body_without_truncation() {
        // 300 KB body —— 大于 `bytes_payload` 的 256 KB 截断阈值,wire-dump
        // 必须落盘完整内容,否则诊断"上下文丢失"时漏掉关键 contents 数组
        let tmp = tempfile::tempdir().expect("tempdir");
        let body = b"{\"contents\":["
            .to_vec()
            .repeat(1)
            .into_iter()
            .chain(b"a".repeat(300 * 1024))
            .chain(b"]}".to_vec())
            .collect::<Vec<u8>>();
        let path = write_wire_dump_to(tmp.path(), &sample_wire_dump_input(body.clone()))
            .expect("wire_dump 应当返回写出的 path");
        let read = std::fs::read_to_string(&path).expect("dump file 必须可读回");
        let v: Value = serde_json::from_str(&read).expect("dump 必须是合法 JSON");
        assert_eq!(v["kind"], "wire_dump");
        assert_eq!(v["request"]["body"]["bytes"], body.len());
        assert_eq!(
            v["request"]["body"]["truncated_bytes"], 0,
            "wire-dump 必须不截断 —— 这是相对 feedback-bundle 256 KB 上限的核心 \
             差异,缺这个 invariant 就没法看完整 contents 数组"
        );
        let content_len = v["request"]["body"]["content"]
            .as_str()
            .expect("utf-8 body content 应当是 string")
            .len();
        assert_eq!(
            content_len,
            body.len(),
            "落盘 content 必须跟 body 一字节不差"
        );
    }

    #[test]
    fn write_wire_dump_to_creates_daily_subdir() {
        // 日级子目录方便用户按"哪天复现的 issue"快速 cd 进去看,不混在一起;
        // trim 也按日独立执行(单日上限 200,不会跨日删错文件)。
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = write_wire_dump_to(tmp.path(), &sample_wire_dump_input(b"{}".to_vec()))
            .expect("write must succeed");
        let parent = path.parent().expect("path 必有 parent");
        let day = Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(
            parent.file_name().and_then(|s| s.to_str()),
            Some(day.as_str()),
            "dump 必须落在 `YYYY-MM-DD` 子目录,parent={parent:?}"
        );
    }
}
