use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Local;
use codex_app_transfer_registry::config_dir;
use serde_json::{json, Value};

const MAX_STORED_BUNDLES: usize = 50;
const MAX_STORED_BODY_BYTES: usize = 256 * 1024;
/// forward-trace jsonl 按天分文件,保留最近 N 天(防无界增长)。
pub(crate) const FORWARD_TRACE_KEEP_DAYS: usize = 7;

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
    bytes_payload_with_len(bytes, bytes.len(), max_bytes)
}

/// 同 [`bytes_payload`],但 `bytes` 可能是**已被上游预截断**的片段(forward-trace 成功
/// 路径的 tee 把响应体 cap 到 256KiB 后才到这里),`true_len` 是原始全长(未预截断时
/// == `bytes.len()`)。用 `true_len` 计 `bytes`/`truncated_bytes`,避免成功路径谎报
/// "未截断"(否则 `bytes.len()==max` 时旧逻辑 `len > max` 为假 → truncated_bytes=0)。
fn bytes_payload_with_len(bytes: &[u8], true_len: usize, max_bytes: usize) -> Value {
    let shown = bytes.len().min(max_bytes);
    let slice = &bytes[..shown];
    let truncated_bytes = true_len.saturating_sub(shown);
    match std::str::from_utf8(slice) {
        Ok(text) => json!({
            "encoding": "utf8",
            "bytes": true_len,
            "truncated_bytes": truncated_bytes,
            "content": text,
        }),
        Err(_) => json!({
            "encoding": "base64",
            "bytes": true_len,
            "truncated_bytes": truncated_bytes,
            "content": STANDARD.encode(slice),
        }),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// forward-trace 诊断(MOC-89):抓 proxy 协议转发**全过程**(Codex 原始请求 inbound
// → adapter 转换后发上游 outbound → 上游回包 response),一请求一行 jsonl,供精修
// adapters/mapper 对照。**默认关**(env gate),仅本地 loopback 落盘。
//
// ⚠️ 定位:这是**开发者诊断**,不是「脱敏后可安全外泄」的功能。forward-trace 的价值
// 就在抓完整 prompt / 代码 / 模型回复,这些**本身敏感且不脱敏**(脱了无诊断价值)。
// 下面的脱敏只挡**结构化 credential**(header token / JSON 里的 api_key 等),正文照留。
// 所以它默认关 + 仅 loopback + 仅本地,绝不随 release 给终端用户开。见 docs/forward-trace.md。
// ───────────────────────────────────────────────────────────────────────────

/// 运行时开关(app 内「诊断模式」UI toggle / 持久化 settings 自启时置位),与 env 并联。
static RUNTIME_TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

/// forward-trace / MCP-trace 总开关。默认**关**(普通用户零影响)。两条启用路径**并联**:
/// ① env `CAS_DIAG_TRACE=1`/`true`(首读缓存,zero-overhead);② 运行时 [`set_forward_trace_enabled`]
/// (app 内「诊断模式」开关 / 启动时按持久化 settings 置位)。任一为真即开。关时转发热路径
/// 仅一次 `OnceLock` load + 一次 atomic load。
pub fn forward_trace_enabled() -> bool {
    static ENV_ENABLED: OnceLock<bool> = OnceLock::new();
    let env_on = *ENV_ENABLED.get_or_init(|| {
        std::env::var("CAS_DIAG_TRACE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    });
    env_on || RUNTIME_TRACE_ENABLED.load(Ordering::Relaxed)
}

/// 运行时开/关诊断采集(app 内 toggle 用)。env 开启的不受影响(env 恒为真)。
pub fn set_forward_trace_enabled(on: bool) {
    RUNTIME_TRACE_ENABLED.store(on, Ordering::Relaxed);
}

/// forward 全过程 trace 入参(全引用,write 内才序列化)。成功路径由 forward.rs 的
/// `TracedStream::Drop` 借自身 owned 字段构造;错误/retry 路径在 handler 内就地构造。
pub struct ForwardTraceInput<'a> {
    // inbound — Codex 发来的原始请求(rewrite/strip 前)
    pub method: &'a str,
    pub client_path: &'a str,
    pub client_query: Option<&'a str>,
    pub inbound_headers: &'a reqwest::header::HeaderMap,
    pub inbound_body: &'a [u8],
    // outbound — adapter 转换后发上游的
    pub upstream_url: &'a str,
    pub outbound_headers: &'a reqwest::header::HeaderMap,
    pub outbound_body: &'a [u8],
    // response — 上游回包(raw,transform_response_stream 之前)
    pub status: u16,
    pub response_headers: &'a reqwest::header::HeaderMap,
    /// 响应体片段。成功路径来自 tee,可能已被 cap 截断(见 `response_full_len`);
    /// 错误/retry 路径是完整 buffer。
    pub response_body: &'a [u8],
    /// 响应体**原始全长**(成功路径 = TracedStream 累计的 total_bytes;错误/retry 路径
    /// = response_body.len())。用于在 jsonl 里如实标注 bytes/truncated_bytes。
    pub response_full_len: usize,
    // meta
    pub provider_id: &'a str,
    pub provider_name: &'a str,
    pub api_format: &'a str,
    pub auth_scheme: &'a str,
    pub original_model: Option<&'a str>,
    pub resolved_model: Option<&'a str>,
    pub upstream_model: Option<&'a str>,
}

/// HeaderMap → JSON `{name: value}`,credential 类 value 脱敏成 `***(len=N)`;协议字段
/// (user-agent / x-goog-api-client / content-type / anthropic-version 等)**全留**(精修
/// 要看真实值)。header 侧用宽匹配(含 `token`/`secret` 子串)——header 名里不会出现像
/// `max_tokens` 这种业务字段,宽匹配安全;body 侧另用归一化匹配(见 [`is_credential_key`])。
pub fn headers_to_json_redacted(h: &reqwest::header::HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in h.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        let sensitive = matches!(
            lower.as_str(),
            "authorization"
                | "proxy-authorization"
                | "api-key"
                | "x-api-key"
                | "openai-api-key"
                | "anthropic-api-key"
                | "x-goog-api-key"
                | "cookie"
                | "set-cookie"
        ) || lower.starts_with("cookie-")
            || lower.starts_with("x-auth-")
            || lower.starts_with("x-csrf-")
            || lower.starts_with("x-session-")
            || lower.contains("secret")
            || lower.contains("token")
            || lower.contains("credential")
            || lower.contains("password");
        let v = if sensitive {
            format!("***(len={})", value.as_bytes().len())
        } else {
            match value.to_str() {
                Ok(s) => s.to_string(),
                Err(_) => format!("<non-utf8 len={}>", value.as_bytes().len()),
            }
        };
        out.entry(name.as_str().to_string())
            .and_modify(|prev| {
                if let Value::Array(arr) = prev {
                    arr.push(Value::String(v.clone()));
                } else {
                    let p = prev.clone();
                    *prev = Value::Array(vec![p, Value::String(v.clone())]);
                }
            })
            .or_insert(Value::String(v));
    }
    Value::Object(out)
}

/// body → JSON payload。对 JSON content-type 且未超 cap 的 body,解析后**精确清洗**
/// 结构化 credential 字段(api_key / authorization / *_token 等 → `***`);其余(SSE /
/// 二进制 / 超大 / 非 JSON)退回 `bytes_payload`(utf8/base64 + cap 报 truncated_bytes)。
///
/// **精确匹配**:不用 `contains("token")` —— 否则会误伤 LLM 的 `max_tokens` 等诊断字段;
/// 也不用 `contains("key")`(误伤 `cache_key` 等)。只清洗明确的 credential 键。
fn redact_body(bytes: &[u8], content_type: Option<&str>) -> Value {
    redact_body_with_len(bytes, bytes.len(), content_type)
}

/// 同 [`redact_body`],但 `bytes` 可能已被预截断(见 [`bytes_payload_with_len`]);`true_len`
/// 是原始全长。仅对**未预截断的完整 JSON**(`true_len == bytes.len()`)做解析 + key 清洗。
///
/// **不按大小跳过解析**:大 JSON(>256KiB,大 prompt/代码常见)必须「**先脱敏、再 cap**」。
/// 否则首 256KiB 里的 `api_key`/`authorization`/`*_token` 会经 bytes 分支原样落盘,违反
/// body 脱敏保证(codex-connector P2)。cap 应用在**脱敏后**的序列化结果上。
///
/// **预截断的 JSON**(成功路径 tee 把响应体 cap 后 `true_len > bytes.len()`):残缺片段
/// 无法解析脱敏,而前缀里可能含 credential → **不落正文、只记元信息**(安全 > 该场景已
/// 因截断而残缺的有限完整性,codex-connector P2 二轮)。非 JSON / SSE 不受影响,照走
/// `bytes_payload_with_len`(正文是模型回复等,无结构化 credential)。
fn redact_body_with_len(bytes: &[u8], true_len: usize, content_type: Option<&str>) -> Value {
    let is_json = content_type
        .map(|c| c.to_ascii_lowercase().contains("json"))
        .unwrap_or(false);
    if is_json && true_len == bytes.len() {
        if let Ok(mut v) = serde_json::from_slice::<Value>(bytes) {
            redact_json_credentials(&mut v);
            // 脱敏后再量大小:未超 cap → 原样发完整脱敏对象;超 cap → 退回**已脱敏**的
            // JSON 文本截断(credential 已 scrub,如实记 truncated_bytes)。
            let serialized = serde_json::to_vec(&v).unwrap_or_default();
            if serialized.len() <= MAX_STORED_BODY_BYTES {
                return json!({
                    "encoding": "json",
                    "bytes": true_len,
                    "truncated_bytes": 0,
                    "content": v,
                });
            }
            return bytes_payload_with_len(&serialized, serialized.len(), MAX_STORED_BODY_BYTES);
        }
    }
    if is_json && true_len != bytes.len() {
        // 预截断的 JSON:无法解析脱敏 + 前缀可能含 credential → 不落正文,只记元信息。
        return json!({
            "encoding": "json_truncated_omitted",
            "bytes": true_len,
            "truncated_bytes": true_len.saturating_sub(bytes.len()),
            "content": Value::Null,
            "note": "large JSON body truncated upstream of redaction; omitted to avoid leaking unredactable credentials",
        });
    }
    bytes_payload_with_len(bytes, true_len, MAX_STORED_BODY_BYTES)
}

/// 递归把 JSON 里的 credential 字段值替换成 `***`。键判定见 [`is_credential_key`]。
fn redact_json_credentials(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_credential_key(k) {
                    *val = Value::String("***".to_string());
                } else {
                    redact_json_credentials(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_json_credentials(item);
            }
        }
        _ => {}
    }
}

/// 判定一个 JSON key 是否 credential。先**归一化**(小写 + 去 `_`/`-`)让 snake/camel/kebab
/// 同形 —— `access_token` / `accessToken` / `access-token` 都 → `accesstoken`,避免 camelCase
/// 漏网(codex-connector P2:`accessToken`/`refreshToken`/`idToken` 小写后无下划线,曾绕过
/// snake-case 精确名 + `_token` 后缀检查而原样落盘)。
///
/// **刻意用 `ends_with("token")`(单数)而非 `contains("token")`**:后者会误伤 `max_tokens`
/// (→ `maxtokens`,结尾是 `tokens` 复数,不命中),诊断要看的字段得以保留;`completion_tokens`
/// / `prompt_tokens` 等用量字段同理(复数)不受影响。`ends_with("apikey")` 覆盖
/// `apiKey`/`x-api-key`/`openai_api_key` 等各写法。
fn is_credential_key(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    norm == "authorization"
        || norm.contains("secret")
        || norm.contains("password")
        || norm.contains("credential")
        || norm.ends_with("token")
        || norm.ends_with("apikey")
}

fn header_content_type(h: &reqwest::header::HeaderMap) -> Option<&str> {
    h.get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
}

/// 把一条 forward 全过程 trace 序列化成**已脱敏**的 JSON 对象(jsonl 行 / SSE / API 共用)。
/// header 脱敏 + body 按 content-type 脱敏/截断;`response` 用 `response_full_len` 修正
/// 预截断(成功路径 tee)的 `truncated_bytes`。
pub(crate) fn build_forward_trace_value(input: &ForwardTraceInput, seq: u64) -> Value {
    json!({
        "trace_kind": "forward_protocol",
        "captured_at": Local::now().to_rfc3339(),
        "proxy_version": env!("CARGO_PKG_VERSION"),
        "seq": seq,
        // ── Codex 发来的原始请求 ──
        "inbound": {
            "method": input.method,
            "client_path": input.client_path,
            "client_query": input.client_query,
            "headers": headers_to_json_redacted(input.inbound_headers),
            "body": redact_body(input.inbound_body, header_content_type(input.inbound_headers)),
        },
        // ── adapter 转换后发上游的 ──
        "outbound": {
            "url": input.upstream_url,
            "headers": headers_to_json_redacted(input.outbound_headers),
            "body": redact_body(input.outbound_body, header_content_type(input.outbound_headers)),
        },
        // ── 上游回包(raw) ──
        "response": {
            "status": input.status,
            "headers": headers_to_json_redacted(input.response_headers),
            "body": redact_body_with_len(
                input.response_body,
                input.response_full_len,
                header_content_type(input.response_headers),
            ),
        },
        "provider": {
            "id": input.provider_id,
            "name": input.provider_name,
            "api_format": input.api_format,
            "auth_scheme": input.auth_scheme,
        },
        "models": {
            "original": input.original_model,
            "resolved": input.resolved_model,
            "upstream": input.upstream_model,
        },
    })
}

/// 抓一条 forward 全过程 trace → 统一 [`crate::trace_store`](ring + broadcast + jsonl)。
/// 调用方须先用 [`forward_trace_enabled`] gate。返回 jsonl 路径(写盘失败 / 无 home 返
/// `None`)供调用方判定「开了诊断却写不出」。ring/broadcast 是 best-effort,不影响返回值。
pub fn write_forward_trace_jsonl(input: &ForwardTraceInput) -> Option<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let value = build_forward_trace_value(input, seq);
    crate::trace_store::trace_store().push(crate::trace_store::TraceKind::Forward, seq, value)
}

fn trim_old_bundles(dir: &Path, keep: usize) {
    trim_old_files(dir, keep, "json");
}

/// 按 mtime 保留最近 `keep` 个指定扩展名的文件,其余删除。bundle(json,按条)与
/// forward-trace(jsonl,按天)共用;后者由 [`crate::trace_store`] 调用。
pub(crate) fn trim_old_files(dir: &Path, keep: usize, ext: &str) {
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for item in rd.flatten() {
        let path = item.path();
        if path.extension().and_then(|v| v.to_str()) != Some(ext) {
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

    #[test]
    fn redact_body_cleanses_credentials_but_keeps_diagnostic_fields() {
        let body = br#"{"model":"gpt-5","max_tokens":4096,"api_key":"sk-secret","nested":{"access_token":"tok","cache_key":"keep"},"messages":[{"authorization":"Bearer x"}]}"#;
        let v = redact_body(body, Some("application/json"));
        assert_eq!(v["encoding"], "json");
        let c = &v["content"];
        // credential 字段被脱敏
        assert_eq!(c["api_key"], "***");
        assert_eq!(c["nested"]["access_token"], "***");
        assert_eq!(c["messages"][0]["authorization"], "***");
        // 诊断要看的字段全留(尤其 max_tokens 不能被 contains("token") 误伤)
        assert_eq!(c["max_tokens"], 4096);
        assert_eq!(c["model"], "gpt-5");
        assert_eq!(c["nested"]["cache_key"], "keep");
    }

    #[test]
    fn redact_body_cleanses_camelcase_and_kebab_credential_keys() {
        // codex-connector P2:camelCase/kebab credential 归一化后也要命中(snake-only 会漏)
        let body = br#"{"accessToken":"a","refreshToken":"b","idToken":"c","clientSecret":"d","apiKey":"e","x-api-key":"f","max_tokens":1,"completion_tokens":2,"total_tokens":3,"token_type":"Bearer"}"#;
        let v = redact_body(body, Some("application/json"));
        let c = &v["content"];
        for k in [
            "accessToken",
            "refreshToken",
            "idToken",
            "clientSecret",
            "apiKey",
            "x-api-key",
        ] {
            assert_eq!(c[k], "***", "{k} 未脱敏");
        }
        // 用量/诊断字段(复数 tokens)不被误伤;token_type 值是 Bearer 非密钥
        assert_eq!(c["max_tokens"], 1);
        assert_eq!(c["completion_tokens"], 2);
        assert_eq!(c["total_tokens"], 3);
        assert_eq!(c["token_type"], "Bearer");
    }

    #[test]
    fn redact_body_redacts_credentials_in_oversized_json() {
        // >256KiB 的完整 JSON,credential 在最前面:必须先脱敏再 cap,不能让首 256KiB
        // 原样落盘(codex-connector P2 回归)。
        let filler = "x".repeat(300 * 1024);
        let body = format!(
            r#"{{"api_key":"sk-LEAK-must-not-appear","authorization":"Bearer LEAK-token","pad":"{filler}"}}"#
        );
        assert!(
            body.len() > MAX_STORED_BODY_BYTES,
            "测试前提:body 必须超 cap"
        );
        let v = redact_body(body.as_bytes(), Some("application/json"));
        let s = serde_json::to_string(&v).unwrap();
        assert!(
            !s.contains("sk-LEAK-must-not-appear"),
            "大 JSON 的 api_key 泄露了"
        );
        assert!(
            !s.contains("Bearer LEAK-token"),
            "大 JSON 的 authorization 泄露了"
        );
        assert!(s.contains("***"), "应含脱敏占位符");
    }

    #[test]
    fn redact_body_omits_pre_truncated_json_to_avoid_leak() {
        // 成功路径 tee:JSON 响应被 cap 到 256KiB(true_len > len),前缀含 credential。
        // 无法解析脱敏 → 必须不落正文(omit),而非把含 api_key 的前缀原样写出。
        let prefix = format!(r#"{{"api_key":"sk-LEAK","data":"{}"#, "y".repeat(1000));
        let true_len = 400 * 1024; // 原始响应远大于 prefix
        let v = redact_body_with_len(prefix.as_bytes(), true_len, Some("application/json"));
        assert_eq!(v["encoding"], "json_truncated_omitted");
        assert!(v["content"].is_null(), "预截断 JSON 不应落正文");
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("sk-LEAK"), "预截断 JSON 的 api_key 泄露了");
        assert_eq!(v["bytes"], true_len);
        assert_eq!(v["truncated_bytes"], true_len - prefix.len());

        // 非 JSON(SSE)预截断不受影响:仍落正文(模型回复,无结构化 credential)
        let sse = redact_body_with_len(b"data: hello", 9999, Some("text/event-stream"));
        assert_eq!(sse["encoding"], "utf8");
    }

    #[test]
    fn redact_body_with_len_reports_truncation_from_pre_capped_buffer() {
        // 模拟成功路径:tee 把 500KiB 响应 cap 到 256KiB,真实全长另传。
        let true_len = 500 * 1024;
        let capped = vec![b'x'; MAX_STORED_BODY_BYTES]; // 已被 tee 截到 256KiB
        let v = redact_body_with_len(&capped, true_len, Some("text/event-stream"));
        // 必须如实报真实全长 + 被丢弃的字节,而非谎报「未截断」
        assert_eq!(v["bytes"], true_len, "bytes 应为原始全长");
        assert_eq!(
            v["truncated_bytes"],
            true_len - MAX_STORED_BODY_BYTES,
            "truncated_bytes 应反映 tee 丢弃量,不能是 0"
        );
        assert_eq!(v["encoding"], "utf8");

        // 未截断时(true_len == len)行为与 redact_body 一致
        let small = redact_body_with_len(b"hello", 5, Some("text/plain"));
        assert_eq!(small["bytes"], 5);
        assert_eq!(small["truncated_bytes"], 0);
    }

    #[test]
    fn redact_body_falls_back_to_bytes_for_non_json() {
        // SSE / 非 JSON 退回 bytes_payload(正文照留,不做 key 清洗)
        let v = redact_body(b"data: {\"x\":1}\n\n", Some("text/event-stream"));
        assert_eq!(v["encoding"], "utf8");
        // content-type 缺失也退回 bytes_payload
        let v2 = redact_body(br#"{"api_key":"sk"}"#, None);
        assert_eq!(v2["encoding"], "utf8");
    }
}
