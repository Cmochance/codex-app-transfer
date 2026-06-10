use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Local;
use codex_app_transfer_registry::config_dir;
use serde_json::{json, Value};

const MAX_STORED_BUNDLES: usize = 50;
const MAX_STORED_BODY_BYTES: usize = 256 * 1024;
/// codex_response(MOC-194)body 上限:比默认大,以**完整逐字节验证大输出的 transfer 转换**
/// (转换后 SSE 常 >256KB;观测过 ~1MB)。仅 codex_response 用,不影响其它 trace 的存储体积。
const MAX_CODEX_RESP_BODY_BYTES: usize = 2 * 1024 * 1024;
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
            "body": redact_bundle_body(&input.request_body),
        },
        "response": {
            "body": redact_bundle_body(&input.response_body),
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

/// bundle(随 feedback 上传)的 request/response body 脱敏(MOC-110 / 安全审计 M-001)。
///
/// **此前用 `bytes_payload` 只编码不 scrub** → 完整请求/响应体里的结构化 credential(JSON 里的
/// `api_key`/`*_token`/JWT 值等)随 feedback 原样外泄(headers 已由 `outbound_headers_redacted`
/// 脱敏,但 body 没有)。改为:尝试当 JSON 解析 → 递归 [`redact_json_credentials`](key + JWT 值)
/// → 序列化(超 cap 截断已脱敏文本);解析失败(SSE / HTML 错误页 / 二进制)退回 `bytes_payload`
/// (那类是模型输出/错误页,无结构化 credential,与 forward-trace `redact_body` 同取舍)。
/// bundle input 不带 content-type,故不依赖 content-type 判别,直接试解析。
///
/// **额外 [`redact_echoed_tokens`]**(MOC-110 / codex P1):上游 401/403 常把 key 回显进
/// `error.message` 这类非凭据字段的字符串值里,key 级 / JWT 级判定抓不到,而 bundle 又随
/// feedback **上传**(区别于仅本地的 viewer)→ 再扫一遍前缀型凭据 token(`sk-…` / `Bearer …`
/// 等)。仅 bundle 路径做,forward-trace 本地 viewer 保「正文照留」契约不调用。
fn redact_bundle_body(bytes: &[u8]) -> Value {
    if let Ok(mut v) = serde_json::from_slice::<Value>(bytes) {
        redact_json_credentials(&mut v);
        redact_echoed_tokens(&mut v);
        let serialized = serde_json::to_vec(&v).unwrap_or_default();
        if serialized.len() <= MAX_STORED_BODY_BYTES {
            return json!({
                "encoding": "json",
                "bytes": bytes.len(),
                "truncated_bytes": 0,
                "content": v,
            });
        }
        return bytes_payload_with_len(&serialized, serialized.len(), MAX_STORED_BODY_BYTES);
    }
    // 非 JSON 退回(HTML 401 页 / 纯文本错误 / 个别坏字节的 mostly-text 页):用 **lossy** UTF-8
    // 解码后扫 echo token —— 不能直接信 `bytes_payload`,因为只要 body 含**任一**非法 UTF-8 字节,
    // 它就把整段标 base64 而跳过脱敏,导致 mostly-text 错误页里回显的 `sk-…`/Bearer 经 base64
    // 原样进 bundle 上传(codex P2)。lossy 把坏字节换 U+FFFD、ASCII 凭据照常可扫;真·二进制
    // lossy 后是乱码、无可读 token、scrub 不命中,无害。
    let lossy = String::from_utf8_lossy(bytes);
    // ① 先复用 `redact_body_string`(MCP 那套):form-urlencoded 键级脱敏(`client_secret`/
    //    `refresh_token` 等无前缀 secret,OAuth token 端点错误体常见)/ 完整 JSON 键脱 / 看似 JSON
    //    但残缺 → 省略占位(codex P2)。② 再 echo-token 扫(纯文本里回显的 sk-/Bearer/JWT)。
    let base = redact_body_string(&lossy).unwrap_or_else(|| lossy.into_owned());
    let scrubbed = redact_credential_tokens(&base).unwrap_or(base);
    bytes_payload(scrubbed.as_bytes(), MAX_STORED_BODY_BYTES)
}

/// 对**已落盘**的 feedback bundle 在**上传前**再脱敏一遍(MOC-110 / codex P1)。
///
/// 写路径([`write_upstream_error_bundle`])只保护**新写**的 bundle;但 feedback 上传的是
/// `recent_feedback_bundles` 的历史文件 —— 用户**升级前**旧 build 写的 bundle 里
/// request/response body 仍是原始未脱敏,升级后下次提交 feedback 会把它原样上传。故上传前
/// 用本函数把每条 bundle 的 `request.body` / `response.body` 段重跑 [`redact_bundle_body`]:
/// 旧 bundle 的 `content` 是原始 body 文本(string)→ 重新解析 + 脱敏;新 bundle 的 `content`
/// 已是脱敏结构 → 重跑幂等无变化。解析失败原样返回(bundle 是本工具写的合法 JSON,理论不触发)。
pub fn rescrub_persisted_bundle(bytes: &[u8]) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<Value>(bytes) else {
        return bytes.to_vec();
    };
    for section in ["request", "response"] {
        let Some(body) = v.get_mut(section).and_then(|s| s.get_mut("body")) else {
            continue;
        };
        // 把 body.content 还原成「原始 body bytes」再脱一遍。旧:content 是原始文本 string
        // (encoding utf8)或 **base64**(原始 body 有非法 UTF-8 时);新:content 是已脱敏
        // object(encoding json),序列化后重跑(幂等)。
        let encoding = body.get("encoding").and_then(Value::as_str);
        let raw: Option<Vec<u8>> = match body.get("content") {
            // 旧 base64 body:必须**先解码**回原始 bytes 再脱敏,否则扫的是 base64 文本、
            // 解码后里面的 sk-/Bearer 漏脱(codex P2)。解码失败退回当文本扫。
            Some(Value::String(s)) if encoding == Some("base64") => Some(
                STANDARD
                    .decode(s)
                    .unwrap_or_else(|_| s.clone().into_bytes()),
            ),
            Some(Value::String(s)) => Some(s.clone().into_bytes()),
            Some(other) if !other.is_null() => serde_json::to_vec(other).ok(),
            _ => None,
        };
        let Some(raw) = raw else { continue };
        // 截断的旧 body(`truncated_bytes>0`)只存了前缀:若它还**不能解析成 JSON**(大请求被
        // 截在闭合括号前),key 级脱敏(`client_secret`/`privateKey` 等非前缀 secret)做不了,
        // echo-scan 也只挡前缀 token → 保守**省略**正文,不冒险半脱敏上传(codex P2;尾部已丢、
        // 无法补全)。能解析 / 完整非 JSON(SSE/HTML)则照常走 redact_bundle_body(全脱 / echo-scrub)。
        let truncated = body
            .get("truncated_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0;
        if truncated && serde_json::from_slice::<Value>(&raw).is_err() {
            *body = json!({
                "encoding": "omitted",
                "bytes": body.get("bytes").cloned().unwrap_or(Value::Null),
                "truncated_bytes": body.get("truncated_bytes").cloned().unwrap_or(Value::Null),
                "content": Value::Null,
                "note": "legacy truncated body omitted before feedback upload (cannot fully redact a truncated, unparseable body)",
            });
        } else {
            *body = redact_bundle_body(&raw);
        }
    }
    serde_json::to_vec_pretty(&v).unwrap_or_else(|_| bytes.to_vec())
}

/// 递归对 JSON 里**所有 string 值**跑 [`redact_credential_tokens`],就地脱敏 echo 进文本的凭据。
/// 仅 [`redact_bundle_body`](feedback 上传路径)调用 —— forward-trace 本地 viewer 不调用。
fn redact_echoed_tokens(v: &mut Value) {
    match v {
        Value::String(s) => {
            if let Some(red) = redact_credential_tokens(s) {
                *s = red;
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(redact_echoed_tokens),
        Value::Object(map) => map.values_mut().for_each(redact_echoed_tokens),
        _ => {}
    }
}

/// 在字符串里就地脱敏「前缀型」凭据 token —— 上游 401/403 常把 key 回显进 `error.message`
/// 这类字符串值(`Invalid API key sk-…` / `Bearer …`),key 级判定抓不到。**只命中「已知前缀 +
/// 足够长的 token 串」**:`sk-` 后需 ≥20 个 token 字符,故 `task-`/`ask-` 等普通正文不会误伤
/// (token 字符 = 字母数字 / `-` / `_` / `.`)。返回 `Some(脱敏后)` / `None`(无命中)。
fn redact_credential_tokens(s: &str) -> Option<String> {
    // (前缀, 前缀后 token 最小长度)。覆盖主流家:OpenAI/Anthropic sk- · Groq gsk_ · xAI xai- ·
    // Google AIza · GitHub ghp_/gho_/ghs_/github_pat_。Bearer 单独处理(token 不一定带前缀)。
    const PREFIXES: &[(&str, usize)] = &[
        ("sk-", 20),
        ("gsk_", 20),
        ("xai-", 20),
        ("AIza", 24),
        ("ghp_", 20),
        ("gho_", 20),
        ("ghs_", 20),
        ("github_pat_", 20),
    ];
    let is_tok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let tok_len = |after: &str| -> usize { after.chars().take_while(|c| is_tok(*c)).count() };
    // Bearer/OAuth opaque token 用 RFC 6750/7235 **token68** 字母表(比 prefix-key 的 is_tok 宽:
    // 含 `~ + /`)+ 末尾可选 `=` padding(base64 标准)。否则含 `+`/`/`/`~`/`=` 的 token 会被
    // is_tok 截断、suffix 漏脱(codex P2)。token68 全 ASCII,char 数 == byte 数。
    let bearer_tok_len = |after: &str| -> usize {
        let main = after
            .chars()
            .take_while(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '+' | '/')
            })
            .count();
        let pad = after.chars().skip(main).take_while(|c| *c == '=').count();
        main + pad
    };

    let mut out = String::with_capacity(s.len());
    let mut changed = false;
    let mut rest = s;
    'scan: while !rest.is_empty() {
        // JWT 子串(`eyJ….eyJ….sig`):嵌在消息文本里(`invalid token eyJ…`)时,值层 looks_like_jwt
        // (判整串)抓不到 → 在此按子串挡(codex P1)。用 jwt_match_len **精确**匹配三段 base64url、
        // 停在签名段末尾,**不吞尾随标点**(句号/逗号/括号等)——否则尾随 `.` 会被算成第 4 段而漏(codex P2)。
        if let Some(n) = jwt_match_len(rest) {
            out.push_str("***");
            rest = &rest[n..];
            changed = true;
            continue 'scan;
        }
        for (pfx, minlen) in PREFIXES {
            if let Some(after) = rest.strip_prefix(*pfx) {
                let n = tok_len(after);
                if n >= *minlen {
                    out.push_str(pfx);
                    out.push_str("***");
                    rest = &after[after.char_indices().nth(n).map_or(after.len(), |(i, _)| i)..];
                    changed = true;
                    continue 'scan;
                }
            }
        }
        // Bearer scheme 大小写不敏感(RFC 6750)→ `bearer`/`BEARER`/`Bearer` 都认;前 7 字节
        // 恒为 ASCII(`bearer `),按字节判可安全切片。保留原始大小写前缀。
        let rb = rest.as_bytes();
        if rb.len() > 7 && rb[..6].eq_ignore_ascii_case(b"bearer") && rb[6] == b' ' {
            let after = &rest[7..];
            let n = bearer_tok_len(after);
            if n >= 16 {
                out.push_str(&rest[..7]);
                out.push_str("***");
                rest = &after[after.char_indices().nth(n).map_or(after.len(), |(i, _)| i)..];
                changed = true;
                continue 'scan;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    changed.then_some(out)
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
// 所以它默认关 + 仅 loopback + 仅本地,绝不随 release 给终端用户开。见 README「协议转发诊断」节。
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
    redact_body_with_cap(bytes, true_len, content_type, MAX_STORED_BODY_BYTES)
}

/// 同 [`redact_body_with_len`],但 body 上限可指定(`cap`)。codex_response(MOC-194)用更大的
/// [`MAX_CODEX_RESP_BODY_BYTES`] 以完整逐字节验证大输出的 transfer 转换;forward/chatgpt-backend
/// 仍走默认 [`MAX_STORED_BODY_BYTES`]。
fn redact_body_with_cap(
    bytes: &[u8],
    true_len: usize,
    content_type: Option<&str>,
    cap: usize,
) -> Value {
    let is_json = content_type
        .map(|c| c.to_ascii_lowercase().contains("json"))
        .unwrap_or(false);
    if is_json && true_len == bytes.len() {
        if let Ok(mut v) = serde_json::from_slice::<Value>(bytes) {
            redact_json_credentials(&mut v);
            // 脱敏后再量大小:未超 cap → 原样发完整脱敏对象;超 cap → 退回**已脱敏**的
            // JSON 文本截断(credential 已 scrub,如实记 truncated_bytes)。
            let serialized = serde_json::to_vec(&v).unwrap_or_default();
            if serialized.len() <= cap {
                return json!({
                    "encoding": "json",
                    "bytes": true_len,
                    "truncated_bytes": 0,
                    "content": v,
                });
            }
            return bytes_payload_with_len(&serialized, serialized.len(), cap);
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
    bytes_payload_with_len(bytes, true_len, cap)
}

/// 递归把 JSON 里的 credential 字段值替换成 `***`。键判定见 [`is_credential_key`];另对
/// **任意 string 值**做 [`looks_like_jwt`] 兜底(MOC-110:JWT 落在非凭据 key 的值里时,
/// 仅靠 key 名判定会漏)。
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
        Value::String(s) if looks_like_jwt(s) => {
            *v = Value::String("***".to_string());
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
/// / `prompt_tokens` 等用量字段同理(复数)不受影响。`apikey` 用 `contains`(非 `ends_with`)覆盖
/// `apiKey`/`x-api-key`/`openai_api_key` + **复数 `apiKeys`/`api_keys`** 容器(收敛后 ends_with
/// 漏复数,旧 feedback `contains` 不回归,codex P2);`apikey` 无 `max_tokens` 那种复数误伤,故可
/// 放宽,token 仍保 `ends_with` 单数。
///
/// `pub`:`src-tauri` 的 feedback 诊断包脱敏(`feedback.rs`)复用同一份判定,避免两套黑名单
/// 各自漏键再次分叉(MOC-110:此前 feedback.rs 与本函数键集合不一致)。
///
/// MOC-110 补漏(安全审计 AP-007/M-001):`privateKey`(`private_key`/`sshPrivateKey` 等 →
/// 含 `privatekey`)、`cf_clearance`(Cloudflare 挑战 cookie 的独立 JSON key 形态)、字面 `jwt`
/// 键。JWT 作为**值**(key 名不带凭据语义时)另由 [`looks_like_jwt`] 在值层兜底。
/// `authorization` 用 `contains`(非精确相等)覆盖 `Proxy-Authorization` / `X-Authorization-*`
/// 前缀型头 —— feedback config 附件脱敏(`feedback.rs`)收敛复用本判定,旧 `contains` 行为不回归
/// (`authScheme`/`author` 不含 `authorization` 子串,不误伤)。
pub fn is_credential_key(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    norm.contains("authorization")
        || norm == "jwt"
        || norm.contains("secret")
        || norm.contains("password")
        || norm.contains("credential")
        || norm.contains("privatekey")
        || norm.contains("cfclearance")
        || norm.ends_with("token")
        // 复数凭据 token 容器:`tokens`/`authTokens`/`accessTokens` 等。**不能**用 `contains("token")`
        // 放宽 —— 会误伤 `max_tokens`/`completion_tokens`/`*_tokens` 用量诊断字段(有意保留、有测试)。
        // 故只列举明确是凭据的复数名(codex P2:收敛后 ends_with 漏复数,但用量复数必须留)。
        || matches!(
            norm.as_str(),
            "tokens"
                | "authtokens"
                | "accesstokens"
                | "refreshtokens"
                | "sessiontokens"
                | "idtokens"
                | "bearertokens"
        )
        || norm.contains("apikey")
}

/// 极窄的 JWT **值**判定:恰好三段点分,前两段以 `eyJ`(base64url of `{"` —— JWT header/payload
/// 必是 JSON 对象)开头。普通文本几乎不可能同时满足,误伤率极低。用于 key 名不带凭据语义
/// (如某字段值里直接塞了 JWT)时在值层兜底脱敏(MOC-110)。
fn looks_like_jwt(s: &str) -> bool {
    let mut segs = s.split('.');
    match (segs.next(), segs.next(), segs.next(), segs.next()) {
        (Some(a), Some(b), Some(c), None) => {
            a.len() >= 8
                && b.len() >= 8
                && c.len() >= 8
                && a.starts_with("eyJ")
                && b.starts_with("eyJ")
        }
        _ => false,
    }
}

/// 若 `s` **以**一个 JWT 开头,返回该 JWT 的精确字节长度(`Some(n)`),否则 `None`。用于
/// [`redact_credential_tokens`] 在**自由文本里**就地挡 echo 的 JWT:精确匹配 `eyJ<b64url>` `.`
/// `eyJ<b64url>` `.` `<b64url>` 三段(base64url = 字母数字 / `-` / `_`,**不含 `.`**),停在签名段
/// 末尾 —— 因此**不吞尾随句子标点**(`eyJ….sig.` / `…sig,` / `…sig)` 都只匹配到 `sig`)。这正是
/// [`looks_like_jwt`](判整串、`.` 当 token 会把尾随句号算成第 4 段)在子串场景下漏掉的(codex P2)。
fn jwt_match_len(s: &str) -> Option<usize> {
    let b64 = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let seg = |t: &str| -> usize { t.chars().take_while(|c| b64(*c)).map(char::len_utf8).sum() };
    // seg1
    if !s.starts_with("eyJ") {
        return None;
    }
    let s1 = seg(s);
    if s1 < 8 || s.as_bytes().get(s1) != Some(&b'.') {
        return None;
    }
    // seg2(payload 也是 JSON → eyJ 开头)
    let r2 = &s[s1 + 1..];
    if !r2.starts_with("eyJ") {
        return None;
    }
    let s2 = seg(r2);
    if s2 < 8 || r2.as_bytes().get(s2) != Some(&b'.') {
        return None;
    }
    // seg3(签名,base64url)
    let r3 = &r2[s2 + 1..];
    let s3 = seg(r3);
    if s3 < 8 {
        return None;
    }
    Some(s1 + 1 + s2 + 1 + s3)
}

/// 脱敏一条 MCP-trace 记录(整个 JSON 对象,MOC-169 增量 4)。
/// ① 递归按 [`is_credential_key`] 清洗所有 credential 键(覆盖 authorization/api_key/*_token 等);
/// ② `req_headers`/`resp_headers` 额外按**宽 header 白名单**清洗 narrow 漏的(`cookie`/`session`/
/// `proxy-authorization` 等会话凭据头);③ body 类字符串字段(`body`/`req_body`/`resp_body`/`data`)
/// 若是 JSON 则解析后键级清洗,**否则按 form-urlencoded 清洗**(覆盖 oauth token 端点的
/// `client_secret`/`refresh_token` 等 `application/x-www-form-urlencoded` 请求体)。
/// **非 JSON / 非 form / 被页内截断的 body 不解析**(同 forward-trace 取舍)—— MCP 采集默认关 +
/// 仅 loopback + 仅本地,勿外传(见 README「协议转发诊断」节)。
pub fn redact_mcp_value(v: &mut Value) {
    redact_json_credentials(v);
    if let Some(obj) = v.as_object_mut() {
        // ② headers 宽匹配补漏(narrow 的 redact_json_credentials 已处理 authorization/api_key/*_token)
        for hk in ["req_headers", "resp_headers"] {
            if let Some(Value::Object(hdrs)) = obj.get_mut(hk) {
                for (name, val) in hdrs.iter_mut() {
                    if is_wide_extra_credential_header(name) {
                        *val = Value::String("***".to_string());
                    }
                }
            }
        }
        // ③ body:JSON → 键级清洗;否则 form-urlencoded → 键级清洗
        for key in ["body", "req_body", "resp_body", "data"] {
            let scrubbed = match obj.get(key) {
                Some(Value::String(s)) => redact_body_string(s),
                _ => None,
            };
            if let Some(scrubbed) = scrubbed {
                obj.insert(key.to_string(), Value::String(scrubbed));
            }
        }
        // ④ URL 的 credential(query ?code=/?access_token=、Google ?key=、OAuth implicit #access_token=、
        //    SPA hash-route #/cb?code= 等),含 fragment 与嵌套 query
        if let Some(Value::String(u)) = obj.get("url") {
            let (red, changed) = redact_credential_params(u);
            if changed {
                obj.insert("url".to_string(), Value::String(red));
            }
        }
    }
}

/// 把 `k=v` 串里 credential 键的值清洗成 `***`,用于 URL(query/fragment)与 form-urlencoded
/// body。按 `?` / `&` / `#` **统一切段**(保留分隔符),故能处理 `path?query#fragment`、SPA
/// hash-route `#/route?access_token=…`(fragment 里还有嵌套 `?query`)、`#access_token=…`(OAuth
/// implicit)、form body `k=v&k=v` 等各种嵌套;非 `k=v` 段(scheme/host/path)原样保留。
/// 值里的 `=`(如 base64 padding)不切,不破坏 value。返回(脱敏后串, 是否有改动)。
/// 判定见 [`is_credential_query_param`]。
fn redact_credential_params(s: &str) -> (String, bool) {
    let mut out = String::with_capacity(s.len());
    let mut changed = false;
    let mut seg_start = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'?' || b == b'&' || b == b'#' {
            push_redacted_param(&s[seg_start..i], &mut out, &mut changed);
            out.push(b as char);
            seg_start = i + 1;
        }
    }
    push_redacted_param(&s[seg_start..], &mut out, &mut changed);
    (out, changed)
}

/// 一个 `?`/`&`/`#`-分隔段:若是 `name=value` 且 name 是 credential → `name=***`,否则原样。
fn push_redacted_param(seg: &str, out: &mut String, changed: &mut bool) {
    if let Some((k, _v)) = seg.split_once('=') {
        if is_credential_query_param(k) {
            out.push_str(k);
            out.push_str("=***");
            *changed = true;
            return;
        }
    }
    out.push_str(seg);
}

/// URL query 参数名是否承载 credential。比 [`is_credential_key`] 多覆盖 URL 特有的 OAuth /
/// API-key 参数:`code`(授权码)/ `code_verifier`(PKCE)/ `key`(Google `?key=<api_key>`)/
/// `sid` / `session*`。诊断里宁可过度脱敏也不漏 token。
fn is_credential_query_param(name: &str) -> bool {
    if is_credential_key(name) {
        return true;
    }
    let norm: String = name
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    matches!(norm.as_str(), "code" | "codeverifier" | "key" | "sid") || norm.contains("session")
}

/// MCP header 名是否属于 [`is_credential_key`] **没覆盖**的会话凭据类(cookie / session /
/// proxy-authorization)。注:`cookie`/`set-cookie` 是浏览器 forbidden header、JS 抓不到,
/// 这里覆盖主要是 `x-session-*` / `proxy-authorization` 这类 JS 可设可读的自定义凭据头。
fn is_wide_extra_credential_header(name: &str) -> bool {
    let norm: String = name
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    norm.contains("cookie") || norm.contains("session") || norm == "proxyauthorization"
}

/// 脱敏一个 body 字符串:① 完整 JSON → 键级清洗;② form-urlencoded(`k=v&k=v`)→ credential
/// 键值清洗;③ **看似 JSON 但解析失败**(被页内 recorder `truncate` 截断 / 残缺)→ 无法安全
/// 脱敏 + 前缀可能含 credential → **正文省略**(codex-connector P2:截断的 JSON 原样落盘会漏前
/// 64KB 里的 token);④ 纯文本(SSE 等,无结构化 credential)→ 返 `None` 原样保留。
fn redact_body_string(s: &str) -> Option<String> {
    // ① 完整 JSON
    if let Ok(mut parsed) = serde_json::from_str::<Value>(s) {
        redact_json_credentials(&mut parsed);
        return Some(serde_json::to_string(&parsed).unwrap_or_else(|_| "***".to_string()));
    }
    let trimmed = s.trim_start();
    // ② form-urlencoded:含 `=`、不以 `{`/`[` 开头(排除残缺 JSON)。即便被截断也能逐对清洗。
    // 复用 redact_credential_params(同 URL),覆盖 code/code_verifier/client_secret 等。
    if s.contains('=') && !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        let (out, touched) = redact_credential_params(s);
        if touched {
            return Some(out);
        }
    }
    // ③ 看似 JSON 却解析失败(截断/残缺),或带页内截断标记 → 无法解析脱敏,正文省略防泄露
    if trimmed.starts_with('{') || trimmed.starts_with('[') || s.contains("<truncated ") {
        return Some(format!(
            "<diagnostic: unparseable/truncated structured body omitted to avoid credential leak ({} bytes)>",
            s.len()
        ));
    }
    // ④ 纯文本(SSE / 非结构化)→ 原样保留
    None
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
    // 与 MCP-trace 共用全局单调 seq(viewer 行主键全局唯一)。
    let seq = crate::trace_store::next_seq();
    let value = build_forward_trace_value(input, seq);
    crate::trace_store::trace_store().push(crate::trace_store::TraceKind::Forward, seq, value)
}

/// 抓一条 **proxy → Codex 转换后响应**(MOC-194)→ 统一 trace_store(`CodexResponse` kind)。
/// `body` 是 adapter `transform_response_stream` 转换后**真正发给 Codex 的字节**(SSE / JSON),
/// 可能被 tee cap 截断(`full_len` 为真实全长)。调用方须先用 [`forward_trace_enabled`] gate。
/// body 走 [`redact_body_with_len`](SSE/非 JSON 退 `bytes_payload`,正文照留 —— 同 forward-trace
/// 取舍:本地诊断、默认关、勿外传)。用于核对转换输出完整性(如每个 apply_patch 的
/// `output_item.added/done` 是否都发到了 Codex)。
pub fn write_codex_response_trace(
    method: &str,
    client_path: &str,
    status: u16,
    content_type: Option<&str>,
    body: &[u8],
    full_len: usize,
) -> Option<PathBuf> {
    let seq = crate::trace_store::next_seq();
    let value = json!({
        "trace_kind": "codex_response",
        "captured_at": Local::now().to_rfc3339(),
        "proxy_version": env!("CARGO_PKG_VERSION"),
        "seq": seq,
        "method": method,
        "client_path": client_path,
        "status": status,
        "body": redact_body_with_cap(body, full_len, content_type, MAX_CODEX_RESP_BODY_BYTES),
    });
    crate::trace_store::trace_store().push(crate::trace_store::TraceKind::CodexResponse, seq, value)
}

// ============ MOC-125 chatgpt-backend passthrough 抓包诊断 ============
// relay 模式下 Codex 的账号/插件/wham/远程控制请求经 proxy 透传 chatgpt.com,走独立诊断 kind
// (`chatgpt_backend`)。重点**保留 cookie 结构**:set-cookie 的 Domain/Path/SameSite 等属性 +
// cookie name 全留、只打码 value(指纹用于跨轮比对) —— 诊断 enroll 200 下发的 session 是否因
// Domain 不匹配 relay host(127.0.0.1)→ Codex 不回带 → GET server 404 → 重新 enroll 死循环。

/// 对敏感值取稳定非可逆短指纹(8 hex),用于**跨请求比对是否同值**(两轮 authorization 是否同
/// token、set-cookie 下发值与下轮 cookie 回带值是否一致),不泄漏原值。FNV-1a 64-bit,跨进程
/// 稳定(不像 `DefaultHasher` 带随机种子)。
fn value_fingerprint(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (h >> 32) as u32)
}

/// 打码单个 `name=value`(cookie pair / set-cookie 主对):保留 name、value → `***(fp=…)`。
fn mask_name_value(pair: &str) -> String {
    match pair.split_once('=') {
        Some((name, val)) => format!(
            "{}=***(fp={})",
            name.trim(),
            value_fingerprint(val.trim().as_bytes())
        ),
        None => pair.trim().to_string(),
    }
}

/// `Cookie:` 请求头脱敏:`a=v1; b=v2` → `a=***(fp=…); b=***(fp=…)`(保留每个 cookie name)。
fn redact_cookie_value(v: &str) -> String {
    v.split(';')
        .map(mask_name_value)
        .collect::<Vec<_>>()
        .join("; ")
}

/// `Set-Cookie:` 响应头脱敏:主对 name 留、value 打码,**所有属性原样保留**
/// (Domain/Path/Expires/Max-Age/SameSite/Secure/HttpOnly)—— 诊断 cookie 是否因 Domain/Path/
/// SameSite 不匹配 relay host 而不被 Codex 回带(MOC-125 enroll/server 404 死循环嫌疑)。
fn redact_set_cookie_value(v: &str) -> String {
    let mut it = v.splitn(2, ';');
    let masked = mask_name_value(it.next().unwrap_or(""));
    match it.next() {
        Some(attrs) => format!("{masked};{attrs}"),
        None => masked,
    }
}

/// `Authorization:` 脱敏:保留 scheme(Bearer/…)+ 长度 + 指纹,打码 token 本体。
fn redact_authorization_value(v: &str) -> String {
    match v.split_once(' ') {
        Some((scheme, tok)) => {
            format!(
                "{scheme} ***(len={}, fp={})",
                tok.len(),
                value_fingerprint(tok.as_bytes())
            )
        }
        None => format!(
            "***(len={}, fp={})",
            v.len(),
            value_fingerprint(v.as_bytes())
        ),
    }
}

/// passthrough 专用 header → JSON:`cookie`/`set-cookie`/`authorization` **保留结构**(name +
/// set-cookie 属性 + 指纹、打码 value);其余 credential(api-key 等)同 [`headers_to_json_redacted`]
/// 打成 `***(len=N)`;协议字段留真值。仅用于 chatgpt-backend 诊断 kind(MOC-125)。
pub fn headers_to_json_passthrough(h: &reqwest::header::HeaderMap) -> Value {
    let mut out = serde_json::Map::new();
    for (name, value) in h.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        let s = value.to_str().ok();
        let v = match (lower.as_str(), s) {
            ("cookie", Some(s)) => redact_cookie_value(s),
            ("set-cookie", Some(s)) => redact_set_cookie_value(s),
            ("authorization", Some(s)) => redact_authorization_value(s),
            _ => {
                let sensitive = matches!(
                    lower.as_str(),
                    "proxy-authorization"
                        | "api-key"
                        | "x-api-key"
                        | "openai-api-key"
                        | "anthropic-api-key"
                        | "x-goog-api-key"
                ) || lower.starts_with("x-auth-")
                    || lower.starts_with("x-csrf-")
                    || lower.starts_with("x-session-")
                    // [review I-1] 对齐基线 headers_to_json_redacted:非标准 cookie 头
                    // (cookie-* / x-cookie / 废弃 set-cookie2 等)也脱敏,不原样落盘。
                    || lower.contains("cookie")
                    || lower.contains("secret")
                    || lower.contains("token")
                    || lower.contains("credential")
                    || lower.contains("password");
                if sensitive {
                    format!("***(len={})", value.as_bytes().len())
                } else {
                    match s {
                        Some(s) => s.to_string(),
                        None => format!("<non-utf8 len={}>", value.as_bytes().len()),
                    }
                }
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

/// passthrough **请求** body 脱敏:补 [`redact_body`] 对 form-urlencoded body 不脱敏的缺口
/// (codex P2)。backend-api 的 OAuth/token POST 常是 `application/x-www-form-urlencoded`,body 里
/// `code=` / `access_token=` / `client_secret=` 走 redact_body 的非 JSON 分支会原样落盘 → 这里
/// 非 JSON 先过 [`redact_body_string`](form 键值用 redact_credential_params 清洗);JSON / 纯文本
/// (无 credential → redact_body_string 返 None)/ 非 utf8 仍由 redact_body 处理(JSON scrub + cap)。
fn redact_passthrough_req_body(bytes: &[u8], content_type: Option<&str>) -> Value {
    let is_json = content_type
        .map(|c| c.to_ascii_lowercase().contains("json"))
        .unwrap_or(false);
    if !is_json {
        if let Ok(s) = std::str::from_utf8(bytes) {
            if let Some(scrubbed) = redact_body_string(s) {
                return json!({
                    "encoding": "form",
                    "bytes": bytes.len(),
                    "truncated_bytes": 0,
                    "content": scrubbed,
                });
            }
        }
    }
    redact_body(bytes, content_type)
}

/// 一条 chatgpt-backend passthrough trace → 诊断 JSON(MOC-125)。结构同 forward
/// (inbound/outbound/response),但 header 用 [`headers_to_json_passthrough`](cookie 友好脱敏)。
pub(crate) fn build_chatgpt_backend_trace_value(input: &ForwardTraceInput, seq: u64) -> Value {
    json!({
        "trace_kind": "chatgpt_backend",
        "captured_at": Local::now().to_rfc3339(),
        "proxy_version": env!("CARGO_PKG_VERSION"),
        "seq": seq,
        "inbound": {
            "method": input.method,
            // [codex P2] query 里的 credential(?code= / ?access_token= / ?key= / ?sid= 等)脱敏 ——
            // backend-api(OAuth callback / wham 等)的 query 可能带 token,原样落 jsonl/viewer 会泄漏。
            "client_path": redact_credential_params(input.client_path).0,
            "client_query": input.client_query.map(|q| redact_credential_params(q).0),
            "headers": headers_to_json_passthrough(input.inbound_headers),
            "body": redact_passthrough_req_body(input.inbound_body, header_content_type(input.inbound_headers)),
        },
        "outbound": {
            "url": redact_credential_params(input.upstream_url).0,
            "headers": headers_to_json_passthrough(input.outbound_headers),
            "body": redact_passthrough_req_body(input.outbound_body, header_content_type(input.outbound_headers)),
        },
        "response": {
            "status": input.status,
            "headers": headers_to_json_passthrough(input.response_headers),
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
    })
}

/// 抓一条 chatgpt-backend passthrough trace → 统一 trace_store(独立 kind `ChatgptBackend`)。
/// 调用方须先用 [`forward_trace_enabled`] gate。返回 jsonl 路径(写盘失败 / 无 home 返 `None`)。
pub fn write_chatgpt_backend_trace(input: &ForwardTraceInput) -> Option<PathBuf> {
    let seq = crate::trace_store::next_seq();
    let value = build_chatgpt_backend_trace_value(input, seq);
    crate::trace_store::trace_store().push(
        crate::trace_store::TraceKind::ChatgptBackend,
        seq,
        value,
    )
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

    // ── [MOC-125 / review] passthrough cookie 友好脱敏:真值绝不落盘 + 结构保留 ──
    #[test]
    fn passthrough_fingerprint_stable_and_distinct() {
        assert_eq!(value_fingerprint(b"abc"), value_fingerprint(b"abc"));
        assert_ne!(value_fingerprint(b"abc"), value_fingerprint(b"abd"));
        assert_eq!(value_fingerprint(b"abc").len(), 8);
        assert!(value_fingerprint(b"abc")
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn passthrough_cookie_keeps_name_masks_value() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("cookie", "session=SECRET123; csrf=TOK456".parse().unwrap());
        let s = serde_json::to_string(&headers_to_json_passthrough(&h)).unwrap();
        assert!(
            !s.contains("SECRET123") && !s.contains("TOK456"),
            "cookie 真值泄漏: {s}"
        );
        assert!(
            s.contains("session=***") && s.contains("csrf=***"),
            "cookie name 应保留: {s}"
        );
    }

    #[test]
    fn passthrough_set_cookie_keeps_attrs_masks_value() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "set-cookie",
            "__Secure-sess=SECRETVAL; Domain=.chatgpt.com; Path=/; HttpOnly; SameSite=Lax"
                .parse()
                .unwrap(),
        );
        let s = serde_json::to_string(&headers_to_json_passthrough(&h)).unwrap();
        assert!(!s.contains("SECRETVAL"), "set-cookie value 泄漏: {s}");
        assert!(s.contains("__Secure-sess=***"), "name 应保留: {s}");
        assert!(
            s.contains("Domain=.chatgpt.com") && s.contains("Path=/") && s.contains("SameSite=Lax"),
            "属性应保留(诊断 Domain 等关键): {s}"
        );
    }

    #[test]
    fn passthrough_authorization_keeps_scheme_masks_token() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("authorization", "Bearer eyJTOKEN_SECRET".parse().unwrap());
        let s = serde_json::to_string(&headers_to_json_passthrough(&h)).unwrap();
        assert!(!s.contains("eyJTOKEN_SECRET"), "token 泄漏: {s}");
        assert!(s.contains("Bearer ***"), "scheme 应保留: {s}");
    }

    #[test]
    fn passthrough_non_standard_cookie_and_credential_still_redacted() {
        // [review I-1] 非标准 cookie 头 + 其它 credential 不得原样落盘;协议头保留真值。
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-cookie", "LEAK1".parse().unwrap());
        h.insert("x-api-key", "LEAK2".parse().unwrap());
        h.insert("user-agent", "codex/1.0".parse().unwrap());
        let s = serde_json::to_string(&headers_to_json_passthrough(&h)).unwrap();
        assert!(!s.contains("LEAK1"), "x-cookie 真值泄漏: {s}");
        assert!(!s.contains("LEAK2"), "x-api-key 真值泄漏: {s}");
        assert!(s.contains("codex/1.0"), "协议头 user-agent 应保留真值");
    }

    #[test]
    fn passthrough_trace_redacts_query_credentials() {
        // [codex P2] client_path / upstream_url 的 query credential 不得原样落 trace。
        let h = reqwest::header::HeaderMap::new();
        let input = ForwardTraceInput {
            method: "GET",
            client_path: "/backend-api/cb?code=AUTHLEAK&access_token=ATLEAK&scope=read",
            client_query: None,
            inbound_headers: &h,
            inbound_body: b"",
            upstream_url: "https://chatgpt.com/backend-api/cb?code=AUTHLEAK&key=GKEYLEAK",
            outbound_headers: &h,
            outbound_body: b"",
            status: 200,
            response_headers: &h,
            response_body: b"",
            response_full_len: 0,
            provider_id: "chatgpt-backend",
            provider_name: "x",
            api_format: "x",
            auth_scheme: "-",
            original_model: None,
            resolved_model: None,
            upstream_model: None,
        };
        let s = serde_json::to_string(&build_chatgpt_backend_trace_value(&input, 1)).unwrap();
        assert!(
            !s.contains("AUTHLEAK") && !s.contains("ATLEAK") && !s.contains("GKEYLEAK"),
            "query credential 泄漏: {s}"
        );
        assert!(
            s.contains("scope=read"),
            "非 credential query(scope)应保留: {s}"
        );
        assert!(s.contains("code=***"), "code 应脱敏: {s}");
    }

    #[test]
    fn passthrough_trace_redacts_form_body() {
        // [codex P2] form-urlencoded request body 的 credential 不得原样落 trace。
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "content-type",
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        let body = b"code=AUTHLEAK&client_secret=SECLEAK&grant_type=authorization_code";
        let input = ForwardTraceInput {
            method: "POST",
            client_path: "/backend-api/token",
            client_query: None,
            inbound_headers: &h,
            inbound_body: body,
            upstream_url: "https://chatgpt.com/backend-api/token",
            outbound_headers: &h,
            outbound_body: body,
            status: 200,
            response_headers: &h,
            response_body: b"",
            response_full_len: 0,
            provider_id: "chatgpt-backend",
            provider_name: "x",
            api_format: "x",
            auth_scheme: "-",
            original_model: None,
            resolved_model: None,
            upstream_model: None,
        };
        let s = serde_json::to_string(&build_chatgpt_backend_trace_value(&input, 1)).unwrap();
        assert!(
            !s.contains("AUTHLEAK") && !s.contains("SECLEAK"),
            "form body credential 泄漏: {s}"
        );
        assert!(
            s.contains("grant_type=authorization_code"),
            "非 credential form param 应保留: {s}"
        );
    }

    #[test]
    fn redact_mcp_value_cleanses_headers_and_oauth_body() {
        // 模拟一条 MCP/oauth fetch 记录:headers 带 authorization,resp_body 是 oauth token JSON
        let mut v = json!({
            "kind": "fetch",
            "url": "https://auth.example.com/oauth/token",
            "req_headers": {"authorization": "Bearer SECRET", "content-type": "application/json"},
            "resp_body": "{\"access_token\":\"at-LEAK\",\"refresh_token\":\"rt-LEAK\",\"expires_in\":3600}",
            "max_tokens": 8
        });
        redact_mcp_value(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        // header credential 脱敏
        assert_eq!(v["req_headers"]["authorization"], "***");
        assert_eq!(
            v["req_headers"]["content-type"], "application/json",
            "协议头保留"
        );
        // body 里的 oauth token 脱敏(JSON 解析后 key 清洗)
        assert!(!s.contains("at-LEAK"), "access_token 泄露: {s}");
        assert!(!s.contains("rt-LEAK"), "refresh_token 泄露");
        // 诊断字段不误伤
        assert_eq!(v["max_tokens"], 8);
        assert_eq!(v["url"], "https://auth.example.com/oauth/token");
    }

    #[test]
    fn redact_mcp_value_cleanses_form_urlencoded_body_and_session_headers() {
        let mut v = json!({
            "kind": "fetch",
            "url": "https://auth.example.com/token",
            "req_headers": {
                "x-session-id": "sess-LEAK",
                "proxy-authorization": "Basic LEAK",
                "content-type": "application/x-www-form-urlencoded"
            },
            "req_body": "grant_type=refresh_token&refresh_token=rt-LEAK&client_secret=cs-LEAK&scope=read"
        });
        redact_mcp_value(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        // 宽 header 白名单补漏(narrow is_credential_key 没覆盖的会话凭据头)
        assert_eq!(v["req_headers"]["x-session-id"], "***", "session 头未脱敏");
        assert_eq!(
            v["req_headers"]["proxy-authorization"], "***",
            "proxy-authorization 未脱敏"
        );
        assert_eq!(
            v["req_headers"]["content-type"],
            "application/x-www-form-urlencoded"
        );
        // form-urlencoded body 的 credential 值脱敏,非 credential 字段保留
        assert!(!s.contains("rt-LEAK"), "form refresh_token 泄露: {s}");
        assert!(!s.contains("cs-LEAK"), "form client_secret 泄露");
        let body = v["req_body"].as_str().unwrap();
        assert!(
            body.contains("grant_type=refresh_token"),
            "非 credential 字段应保留"
        );
        assert!(body.contains("scope=read"));
    }

    #[test]
    fn redact_mcp_value_redacts_url_query_credentials() {
        let mut v = json!({
            "kind": "fetch",
            "url": "https://auth.example.com/callback?code=AUTH-LEAK&access_token=AT-LEAK&key=GKEY-LEAK&state=xyz&page=2"
        });
        redact_mcp_value(&mut v);
        let u = v["url"].as_str().unwrap();
        assert!(!u.contains("AUTH-LEAK"), "oauth code 泄露: {u}");
        assert!(!u.contains("AT-LEAK"), "access_token 泄露");
        assert!(!u.contains("GKEY-LEAK"), "google ?key= 泄露");
        // 非 credential 参数 + path 保留
        assert!(u.contains("state=xyz"), "非 credential state 应保留");
        assert!(u.contains("page=2"));
        assert!(u.starts_with("https://auth.example.com/callback?"));
        // 无 query 的 URL 原样
        let mut v2 = json!({"kind":"fetch","url":"https://x.com/mcp"});
        redact_mcp_value(&mut v2);
        assert_eq!(v2["url"], "https://x.com/mcp");

        // OAuth implicit flow:token 在 URL fragment(#access_token=…)也要脱敏(codex-connector)
        let mut v3 = json!({"kind":"ws_open","url":"https://app/cb?state=ok#access_token=FRAG-LEAK&token_type=Bearer&expires_in=3600"});
        redact_mcp_value(&mut v3);
        let u3 = v3["url"].as_str().unwrap();
        assert!(
            !u3.contains("FRAG-LEAK"),
            "fragment access_token 泄露: {u3}"
        );
        assert!(
            u3.contains("token_type=Bearer"),
            "非 credential fragment 参数保留"
        );
        assert!(u3.contains("state=ok"), "query 段保留");
        assert!(u3.contains('#'), "fragment 分隔符保留");

        // SPA hash-route:fragment 里还有嵌套 ?query(#/callback?code=…&access_token=…)
        let mut v4 = json!({"kind":"fetch","url":"https://app/#/oauth/callback?code=HASH-LEAK&access_token=HA-LEAK&tab=1"});
        redact_mcp_value(&mut v4);
        let u4 = v4["url"].as_str().unwrap();
        assert!(!u4.contains("HASH-LEAK"), "hash-route code 泄露: {u4}");
        assert!(!u4.contains("HA-LEAK"), "hash-route access_token 泄露");
        assert!(u4.contains("/oauth/callback"), "hash-route path 保留");
        assert!(u4.contains("tab=1"), "非 credential 参数保留");
    }

    #[test]
    fn redact_mcp_value_omits_truncated_json_body() {
        // 页内 recorder 把超 64KB 的 JSON body 截断并加标记;到这里无法解析 → 正文省略防泄露
        let mut v = json!({
            "kind": "fetch",
            "resp_body": "{\"access_token\":\"at-LEAK\",\"data\":\"xxxxxxxxxx...<truncated 99999 bytes>"
        });
        redact_mcp_value(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("at-LEAK"), "截断 JSON 的 token 泄露了: {s}");
        assert!(
            v["resp_body"].as_str().unwrap().contains("omitted"),
            "截断 JSON 正文应省略"
        );
        // 纯文本(SSE)不受影响,原样保留
        let mut sse = json!({"kind":"ws_recv","data":"event: ping\ndata: hello\n\n"});
        redact_mcp_value(&mut sse);
        assert_eq!(sse["data"], "event: ping\ndata: hello\n\n");
    }

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

    // 一段形态合法的 JWT(header/payload 以 eyJ 开头,三段点分),仅用于测试值层兜底。
    const FAKE_JWT: &str =
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1g";

    #[test]
    fn is_credential_key_covers_moc110_keys() {
        // MOC-110 补的:privateKey / private_key / sshPrivateKey / jwt / cf_clearance
        for k in [
            "privateKey",
            "private_key",
            "sshPrivateKey",
            "jwt",
            "cf_clearance",
            "cfClearance",
        ] {
            assert!(is_credential_key(k), "{k} 应判为 credential key");
        }
        // 既有覆盖不回归 + 前缀型 authorization 头(contains)+ 复数 apiKeys 容器(codex P2)
        for k in [
            "authorization",
            "Proxy-Authorization",
            "X-Authorization-Token",
            "api_key",
            "apiKeys",
            "api_keys",
            "accessToken",
            "client_secret",
            "tokens",
            "authTokens",
            "accessTokens",
        ] {
            assert!(is_credential_key(k), "{k} 应判为 credential key");
        }
        // 诊断/用量字段不误伤:max_tokens / *_tokens 复数用量字段必须保留(不能因复数 token 修复被误伤);
        // authScheme/author 不含 authorization 子串
        for k in [
            "max_tokens",
            "completion_tokens",
            "prompt_tokens",
            "total_tokens",
            "model",
            "cache_key",
            "wire_api",
            "authScheme",
            "author",
        ] {
            assert!(!is_credential_key(k), "{k} 不应被误判为 credential");
        }
    }

    #[test]
    fn looks_like_jwt_detects_only_real_jwt() {
        assert!(looks_like_jwt(FAKE_JWT));
        // 非 JWT:段太短 / 前缀不对 / 段数不对 / 普通文本
        assert!(!looks_like_jwt("a.b.c"));
        assert!(!looks_like_jwt("foo.bar.baz.qux"));
        assert!(!looks_like_jwt("https://example.com/a.b.c"));
        assert!(!looks_like_jwt("just some normal sentence."));
        assert!(!looks_like_jwt("eyJonly.oneeyJ")); // 仅两段
    }

    #[test]
    fn redact_json_credentials_redacts_jwt_value_under_innocuous_key() {
        // key 名不带凭据语义(note),但值是 JWT → 值层兜底脱敏
        let mut v = json!({"note": FAKE_JWT, "msg": "hello world", "n": 3});
        redact_json_credentials(&mut v);
        assert_eq!(v["note"], "***", "JWT 值应被脱敏");
        assert_eq!(v["msg"], "hello world", "普通文本不动");
        assert_eq!(v["n"], 3);
    }

    #[test]
    fn redact_bundle_body_scrubs_json_and_falls_back_for_non_json() {
        // JSON body:key 级(api_key/privateKey)+ 值级(JWT)都脱敏,诊断字段保留
        let body = format!(
            r#"{{"model":"gpt-5","max_tokens":4096,"api_key":"sk-LEAK","nested":{{"privateKey":"PK-LEAK"}},"note":"{FAKE_JWT}"}}"#
        );
        let v = redact_bundle_body(body.as_bytes());
        assert_eq!(v["encoding"], "json");
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("sk-LEAK"), "api_key 泄漏: {s}");
        assert!(!s.contains("PK-LEAK"), "privateKey 泄漏");
        assert!(!s.contains(FAKE_JWT), "JWT 值泄漏");
        assert_eq!(v["content"]["max_tokens"], 4096, "诊断字段保留");
        assert_eq!(v["content"]["model"], "gpt-5");

        // 非 JSON(SSE / 错误页)退回 bytes_payload,正文原样(无结构化 credential)
        let sse = redact_bundle_body(b"data: {\"delta\":\"hi\"}\n\n");
        assert_eq!(sse["encoding"], "utf8");
    }

    #[test]
    fn redact_bundle_body_scrubs_echoed_credential_tokens() {
        // 上游 401 把 key 回显进 error.message(非凭据 key 的字符串值)→ bundle 路径要挡(codex P1)
        let body = br#"{"error":{"message":"Incorrect API key provided: sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 here"}}"#;
        let v = redact_bundle_body(body);
        let s = serde_json::to_string(&v).unwrap();
        assert!(
            !s.contains("sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"),
            "echo 的 key 泄漏: {s}"
        );
        assert!(s.contains("sk-***"), "应保留前缀 + 脱敏: {s}");

        // 普通正文不误伤:`task-list`/`ask-me` 含 `sk-` 子串但前缀后 token 长度不足
        let benign =
            redact_bundle_body(br#"{"messages":[{"content":"create a task-list and ask-me"}]}"#);
        let bs = serde_json::to_string(&benign).unwrap();
        assert!(bs.contains("task-list"), "普通正文被误伤: {bs}");
        assert!(bs.contains("ask-me"));

        // Bearer 不透明 token 也挡
        let bearer =
            redact_bundle_body(br#"{"detail":"auth failed Bearer abcdef0123456789ghijkl now"}"#);
        let brs = serde_json::to_string(&bearer).unwrap();
        assert!(
            !brs.contains("abcdef0123456789ghijkl"),
            "Bearer token 泄漏: {brs}"
        );
        assert!(brs.contains("Bearer ***"));

        // bearer 大小写不敏感(RFC 6750):小写 `bearer` 也挡,保留原大小写前缀
        let low =
            redact_bundle_body(br#"{"detail":"invalid auth: bearer abcdef0123456789ghijkl end"}"#);
        let ls = serde_json::to_string(&low).unwrap();
        assert!(
            !ls.contains("abcdef0123456789ghijkl"),
            "小写 bearer token 泄漏: {ls}"
        );
        assert!(ls.contains("bearer ***"), "应保留原大小写前缀: {ls}");

        // Bearer opaque token 含 token68 字符(+ / ~ = padding)整段脱敏(RFC 6750/7235,codex P2)
        let b68 = redact_bundle_body(br#"{"d":"Bearer ab+cd/ef~gh0123456789KLMN== rest"}"#);
        let b68s = serde_json::to_string(&b68).unwrap();
        assert!(
            !b68s.contains("ab+cd/ef~gh0123456789KLMN"),
            "token68 Bearer 漏脱: {b68s}"
        );
        assert!(b68s.contains("Bearer ***"), "应整段脱敏: {b68s}");
        assert!(b68s.contains("rest"), "token 后正文保留");

        // form-urlencoded 非 JSON body(OAuth token 端点错误):无前缀 secret 走键级脱敏(codex P2)
        let form = redact_bundle_body(
            b"grant_type=refresh_token&refresh_token=rt-LEAK-val&client_secret=cs-LEAK-val",
        );
        let fs = serde_json::to_string(&form).unwrap();
        assert!(!fs.contains("rt-LEAK-val"), "form refresh_token 泄漏: {fs}");
        assert!(!fs.contains("cs-LEAK-val"), "form client_secret 泄漏: {fs}");
        assert!(
            fs.contains("grant_type=refresh_token"),
            "非凭据 form 字段应保留: {fs}"
        );

        // 非 JSON 上游回包(HTML 401 页 / 纯文本)里回显的 key 走 bytes_payload fallback 也要挡
        let html = redact_bundle_body(
            b"<html>401 Unauthorized: key sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 invalid</html>",
        );
        assert_eq!(html["encoding"], "utf8", "非 JSON 应走 bytes fallback");
        let hs = serde_json::to_string(&html).unwrap();
        assert!(
            !hs.contains("sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"),
            "非 JSON fallback key 泄漏: {hs}"
        );
        assert!(hs.contains("sk-***"));

        // 消息文本里**嵌**的 JWT 子串(非整串 JWT)也挡,且保留上下文(codex P1)
        let jwt_body = format!(r#"{{"error":{{"message":"invalid token {FAKE_JWT} provided"}}}}"#);
        let jv = redact_bundle_body(jwt_body.as_bytes());
        let js = serde_json::to_string(&jv).unwrap();
        assert!(!js.contains(FAKE_JWT), "嵌入的 JWT 泄漏: {js}");
        assert!(
            js.contains("invalid token *** provided"),
            "应只脱敏 JWT 子串、保留上下文: {js}"
        );

        // JWT 紧跟句末标点(句号/逗号)也挡,且标点**保留**(codex P2:精确匹配不吞尾随 `.`)
        let jwt_punct = format!(r#"{{"m":"token {FAKE_JWT}. next {FAKE_JWT}, end"}}"#);
        let jp = serde_json::to_string(&redact_bundle_body(jwt_punct.as_bytes())).unwrap();
        assert!(!jp.contains(FAKE_JWT), "句末 JWT 泄漏: {jp}");
        assert!(jp.contains("token ***. next"), "尾随句号应保留: {jp}");
        assert!(
            jp.contains("*** , end") || jp.contains("***, end"),
            "尾随逗号应保留: {jp}"
        );

        // 含非法 UTF-8 字节的 mostly-text 错误页:lossy 解码后仍脱敏(不能因坏字节整体 base64 跳过)
        let mut non_utf8 = vec![0xff, 0xfe];
        non_utf8.extend_from_slice(b" 401: key sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 end");
        let nv = redact_bundle_body(&non_utf8);
        assert_eq!(nv["encoding"], "utf8", "lossy 后应当 utf8 存而非 base64");
        let ns = serde_json::to_string(&nv).unwrap();
        assert!(
            !ns.contains("sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"),
            "非 UTF-8 fallback key 泄漏: {ns}"
        );
        assert!(ns.contains("sk-***"));
    }

    #[test]
    fn rescrub_persisted_bundle_scrubs_legacy_unredacted_body() {
        // 模拟旧 build 写的 bundle:body.content 是**原始未脱敏** body 文本(string),
        // 含 api_key 值 + echo 进 note 的 key。上传前 rescrub 必须把它们脱掉。
        let legacy = json!({
            "kind": "upstream_error_bundle",
            "request": { "body": {
                "encoding": "utf8", "bytes": 110, "truncated_bytes": 0,
                "content": "{\"model\":\"x\",\"api_key\":\"sk-LEAK-secret-value-1234\",\"note\":\"bad key sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 here\"}"
            }},
            "response": { "body": {
                "encoding": "utf8", "bytes": 24, "truncated_bytes": 0,
                "content": "{\"error\":\"unauthorized\"}"
            }}
        });
        let out = rescrub_persisted_bundle(&serde_json::to_vec(&legacy).unwrap());
        let s = String::from_utf8_lossy(&out);
        assert!(
            !s.contains("sk-LEAK-secret-value-1234"),
            "旧 bundle api_key 泄漏: {s}"
        );
        assert!(
            !s.contains("sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"),
            "旧 bundle echo key 泄漏: {s}"
        );
        assert!(s.contains("unauthorized"), "非凭据内容保留");
        // 非 bundle / 解析失败 → 原样返回,不阻断上传
        assert_eq!(rescrub_persisted_bundle(b"not json"), b"not json");

        // 旧 base64 body(原始含非法 UTF-8 被整体 base64):rescrub 要**先解码**再脱(codex P2)
        let secret = "{\"error\":\"bad key sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 x\"}";
        let b64 = STANDARD.encode(secret);
        let legacy_b64 = json!({
            "kind": "upstream_error_bundle",
            "request": { "body": {"encoding":"json","bytes":2,"truncated_bytes":0,"content":{}} },
            "response": { "body": {"encoding":"base64","bytes": secret.len(),"truncated_bytes":0,"content": b64} }
        });
        let out2 = rescrub_persisted_bundle(&serde_json::to_vec(&legacy_b64).unwrap());
        let s2 = String::from_utf8_lossy(&out2);
        assert!(
            !s2.contains("sk-proj-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"),
            "base64 旧 bundle key 泄漏: {s2}"
        );
        assert!(!s2.contains(&b64), "原始可解码 base64 payload 仍在: {s2}");

        // 截断的旧 body(无法解析 → key 级脱敏做不了)→ 保守省略,不半脱敏上传(codex P2)
        let truncated_legacy = json!({
            "kind": "upstream_error_bundle",
            "request": { "body": {
                "encoding": "utf8", "bytes": 400000, "truncated_bytes": 399000,
                "content": "{\"client_secret\":\"cs-LEAK-no-prefix-value\",\"messages\":[{\"content\":\"aaa"
            }},
            "response": { "body": {"encoding":"utf8","bytes":8,"truncated_bytes":0,"content":"{\"ok\":1}"} }
        });
        let out3 = rescrub_persisted_bundle(&serde_json::to_vec(&truncated_legacy).unwrap());
        let s3 = String::from_utf8_lossy(&out3);
        assert!(
            !s3.contains("cs-LEAK-no-prefix-value"),
            "截断 body 的 client_secret 泄漏: {s3}"
        );
        assert!(s3.contains("omitted"), "截断且无法解析的 body 应省略: {s3}");
    }
}
