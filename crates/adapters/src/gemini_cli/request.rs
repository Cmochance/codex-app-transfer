//! Cloud Code outer envelope 包装。
//!
//! gemini_native 已经产出 Gemini wire 形态的 inner body(contents / tools /
//! systemInstruction / generationConfig 等),Cloud Code Assist 在外层多包一层:
//!
//! ```json
//! {
//!   "model": "<gemini model id>",
//!   "project": "<cloudaicompanion project id>",
//!   "user_prompt_id": "<uuid v4 — 每轮请求新建>",
//!   "request": { /* gemini_native 输出的 inner body */ }
//! }
//! ```
//!
//! `user_prompt_id` 是 Google 内部 telemetry 字段(对齐 gemini-cli `converter.ts:113-119`,
//! 每轮请求新建一个 v4 UUID,服务端用它做 dedupe / tracing)。
//! `session_id` 是 inner 字段(可选,gemini-cli 会注入,但我们暂不强制 — 上游
//! 不传也接受)。

use serde_json::{json, Value};

/// 用 OS RNG 生成 UUID v4 字符串(`8-4-4-4-12` hex 形态)。
///
/// 不依赖 `uuid` crate(避免 workspace 多一个依赖,且 v4 算法很简单)— 16 字节
/// 随机数,按 RFC 4122 §4.4 设 version=4 + variant=10。
///
/// **2026-05-11 critical 修(silent-failure-hunter H5)**:原版用 `let _ =
/// getrandom::getrandom(&mut bytes)` 吞 RNG 失败,失败时所有请求拿同一个 zero
/// UUID `00000000-0000-4000-8000-000000000000`,Google 上游可能 dedupe 导致
/// 第二个请求开始全 silent drop。改成 fallible — RNG 失败直接报错传播给
/// adapter 的 prepare_request,转成 BadRequest 让 client 看到失败而不是
/// silent stuck。
fn uuid_v4() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)?;
    // version 4(top 4 bits of byte 6 = 0100)
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // variant 10(top 2 bits of byte 8 = 10)
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ))
}

/// 把 gemini_native 产出的 inner body 包成 Cloud Code outer envelope。
///
/// 字段顺序对齐 gemini-cli `converter.ts:113-119` 输出:`model` → `project` →
/// `user_prompt_id` → `request`(虽然 JSON 不强 require 顺序,保持一致便于 wire
/// diff 调试)。
///
/// **错误**(2026-05-11 critical 修):RNG 失败时返 Err 而非 silent zero UUID。
/// 调用方(`GeminiCliAdapter::prepare_request`)应转 `AdapterError::BadRequest` 让
/// client 看到失败,不进 silent dedupe 路径。
pub fn wrap_cloud_code_envelope(
    model: &str,
    project_id: &str,
    inner: Value,
) -> Result<Value, getrandom::Error> {
    Ok(json!({
        "model": model,
        "project": project_id,
        "user_prompt_id": uuid_v4()?,
        "request": inner,
    }))
}

/// Antigravity 专属 body 后处理 — **1:1 移植** CLIProxyAPI
/// `antigravity_executor.go::geminiToAntigravity` (line 2326-2360)。
///
/// 在 [`wrap_cloud_code_envelope`] 出来的 envelope 上**追加** antigravity
/// 必需的 4 个字段(否则 Google 上游识别成 non-canonical client → 429 / 配额错):
/// - `userAgent: "antigravity"` (top-level,字面量"antigravity")
/// - `requestType: "agent"` (非 image 模型) 或 `"image_gen"` (model id 含 "image")
/// - `requestId: "agent-<uuid>"` 或 `"image_gen/<ms>/<uuid>/12"`
/// - `request.sessionId: "-<int64-from-sha256-of-first-user-message>"` (非 image 模型)
///
/// 同时:
/// - 删 `request.safetySettings`(antigravity 不接受)
/// - 顶层 `toolConfig` 搬到 `request.toolConfig`(如不存在)—— wrap 完才做,
///   防 outer envelope 错位
///
/// 来源:CLIProxyAPI `internal/runtime/executor/antigravity_executor.go:2326-2360`
pub fn apply_antigravity_transform(
    mut envelope: Value,
    model: &str,
) -> Result<Value, getrandom::Error> {
    let is_image = model.contains("image");

    let envelope_obj = envelope
        .as_object_mut()
        .ok_or_else(|| getrandom::Error::from(std::num::NonZeroU32::new(1).unwrap()))?;

    envelope_obj.insert("userAgent".into(), Value::String("antigravity".into()));
    envelope_obj.insert(
        "requestType".into(),
        Value::String(if is_image { "image_gen" } else { "agent" }.into()),
    );

    // requestId: image_gen 形态带时间戳 + uuid + 固定后缀 12;agent 形态简单 "agent-<uuid>"
    let request_id = if is_image {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("image_gen/{}/{}/12", now_ms, uuid_v4()?)
    } else {
        format!("agent-{}", uuid_v4()?)
    };
    envelope_obj.insert("requestId".into(), Value::String(request_id));

    // request 子对象 mutation:删 safetySettings、加 sessionId(非 image)
    if let Some(request_obj) = envelope_obj
        .get_mut("request")
        .and_then(|v| v.as_object_mut())
    {
        request_obj.remove("safetySettings");

        if !is_image {
            // sessionId:从 request.contents 第一条 role==user 的 parts.0.text
            // 取 SHA256,前 8 byte 作 int64 正值,prefix "-"。CLIProxyAPI
            // `generateStableSessionID` 算法
            let session_id = stable_session_id_from_request(request_obj);
            request_obj.insert("sessionId".into(), Value::String(session_id));
        }
    }

    // toolConfig 顶层 → request.toolConfig 搬迁(antigravity 期望 toolConfig
    // 在 request 子对象内;如果 request.toolConfig 已存在则不搬)
    let top_tool_config = envelope_obj.get("toolConfig").cloned();
    if let Some(tc) = top_tool_config {
        let req_has_tc = envelope_obj
            .get("request")
            .and_then(|v| v.as_object())
            .map(|r| r.contains_key("toolConfig"))
            .unwrap_or(false);
        if !req_has_tc {
            if let Some(request_obj) = envelope_obj
                .get_mut("request")
                .and_then(|v| v.as_object_mut())
            {
                request_obj.insert("toolConfig".into(), tc);
            }
            envelope_obj.remove("toolConfig");
        }
    }

    Ok(envelope)
}

/// 从 request 对象拿第一条 user message text,SHA256 → int64 正值 → "-<n>"。
/// 找不到 user message 时返时间戳-based 退化值(避免 panic)
fn stable_session_id_from_request(request_obj: &serde_json::Map<String, Value>) -> String {
    let text = request_obj
        .get("contents")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter().find_map(|content| {
                if content.get("role").and_then(|r| r.as_str()) == Some("user") {
                    content
                        .get("parts")
                        .and_then(|p| p.as_array())
                        .and_then(|parts| parts.first())
                        .and_then(|p0| p0.get("text"))
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_owned())
                } else {
                    None
                }
            })
        });

    if let Some(t) = text {
        let hash = sha256_first_8_bytes(t.as_bytes());
        // 高 bit 清零保证正(对齐 CLIProxyAPI `& 0x7FFFFFFFFFFFFFFF`)
        let n = i64::from_be_bytes(hash) & 0x7FFFFFFFFFFFFFFFi64;
        return format!("-{n}");
    }
    // fallback:时间戳(不太常见的代码路径,但避免 0)
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64 & 0x7FFFFFFFFFFFFFFFi64)
        .unwrap_or(1);
    format!("-{now}")
}

/// SHA-256 取前 8 byte。不引 sha2 crate 避免新依赖 — 手动实现 SHA-256。
/// 标准 RFC 6234 算法,无 unsafe。
fn sha256_first_8_bytes(input: &[u8]) -> [u8; 8] {
    let full = sha256(input);
    let mut out = [0u8; 8];
    out.copy_from_slice(&full[..8]);
    out
}

/// SHA-256 实现(RFC 6234)。手动 — 避免新引 sha2 crate
fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // padding
    let bit_len = (message.len() as u64) * 8;
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v4_format_matches_rfc_4122() {
        let id = uuid_v4().unwrap();
        assert_eq!(id.len(), 36, "8-4-4-4-12 = 36 chars");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // version digit 必须 4
        assert!(
            parts[2].starts_with('4'),
            "version 应是 4(byte 6 top nibble),实际 {}",
            parts[2]
        );
        // variant — parts[3] 第一个字符必须 8/9/a/b
        let variant_char = parts[3].chars().next().unwrap();
        assert!(
            matches!(variant_char, '8' | '9' | 'a' | 'b'),
            "variant 应 10xx,实际 {variant_char}"
        );
    }

    #[test]
    fn uuid_v4_is_random_each_call() {
        let a = uuid_v4().unwrap();
        let b = uuid_v4().unwrap();
        assert_ne!(a, b, "UUID v4 必须每次不同");
    }

    #[test]
    fn uuid_v4_returns_result_not_silent_zero() {
        // **Critical** silent-failure 修(H5):原版 `let _ = getrandom::getrandom`
        // 吞 RNG 失败 → 所有请求拿同一个 zero UUID。新版返 Result。本测试 lock
        // 签名,防 future regression 重新引入 silent path。
        let result: Result<String, _> = uuid_v4();
        // 现实环境 OS RNG 几乎不可能失败,这里只 lock signature
        let id = result.expect("OS RNG must work in test environment");
        assert_ne!(
            id, "00000000-0000-4000-8000-000000000000",
            "RNG 必须返非零字节(防 silent zero UUID)"
        );
    }

    #[test]
    fn wrap_envelope_preserves_inner_intact() {
        let inner = json!({
            "contents": [{"role":"user","parts":[{"text":"hi"}]}],
            "systemInstruction": {"role":"system","parts":[{"text":"sys"}]},
            "generationConfig": {"temperature": 0.7}
        });
        let wrapped =
            wrap_cloud_code_envelope("gemini-2.5-pro", "proj-abc", inner.clone()).unwrap();
        // inner 必须完全保留
        assert_eq!(wrapped["request"], inner);
        // outer 字段
        assert_eq!(wrapped["model"], "gemini-2.5-pro");
        assert_eq!(wrapped["project"], "proj-abc");
        assert!(wrapped["user_prompt_id"].is_string());
        let upid = wrapped["user_prompt_id"].as_str().unwrap();
        assert_eq!(upid.len(), 36);
    }

    #[test]
    fn wrap_envelope_handles_empty_inner() {
        // 极端情况:inner 是空对象(prepare_request 不该这么调,但我们防御)
        let wrapped = wrap_cloud_code_envelope("g", "p", json!({})).unwrap();
        assert_eq!(wrapped["request"], json!({}));
        assert_eq!(wrapped["model"], "g");
    }

    /// SHA-256 实现锚定 — 用 RFC 6234 标准测试向量(空字符串 + "abc")。
    /// 防 future modify SHA-256 算法引入 silent regression 让 sessionId 偏离
    /// CLIProxyAPI generateStableSessionID 行为
    #[test]
    fn sha256_matches_rfc_6234_test_vectors() {
        // empty string SHA-256
        let empty = sha256(b"");
        let empty_hex: String = empty.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            empty_hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // "abc" SHA-256 — RFC 6234 §A.1 test vector
        let abc = sha256(b"abc");
        let abc_hex: String = abc.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            abc_hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// antigravity transform 加 4 个必需字段 + 删 safetySettings + 移 toolConfig
    #[test]
    fn antigravity_transform_adds_required_fields_for_text_model() {
        let envelope = json!({
            "model": "gemini-3-pro-low",
            "project": "proj-x",
            "user_prompt_id": "upid-1",
            "request": {
                "contents": [
                    {"role":"user","parts":[{"text":"hello world"}]}
                ],
                "safetySettings": [{"category":"SAFETY_X","threshold":"BLOCK_NONE"}]
            },
            "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}}
        });
        let out = apply_antigravity_transform(envelope, "gemini-3-pro-low").unwrap();

        // userAgent + requestType + requestId
        assert_eq!(out["userAgent"], "antigravity");
        assert_eq!(out["requestType"], "agent");
        let request_id = out["requestId"].as_str().unwrap();
        assert!(
            request_id.starts_with("agent-"),
            "non-image model 用 agent-<uuid> 形态,实际 {request_id}"
        );

        // request.sessionId stable hash
        let session_id = out["request"]["sessionId"].as_str().unwrap();
        assert!(
            session_id.starts_with("-"),
            "sessionId 必须以 - 开头(对齐 CLIProxyAPI generateStableSessionID),实际 {session_id}"
        );
        // 同样输入应得同样 sessionId(stable)
        let envelope2 = json!({
            "model": "gemini-3-pro-low",
            "project": "proj-y",
            "user_prompt_id": "upid-2",
            "request": {
                "contents": [{"role":"user","parts":[{"text":"hello world"}]}]
            }
        });
        let out2 = apply_antigravity_transform(envelope2, "gemini-3-pro-low").unwrap();
        assert_eq!(
            out["request"]["sessionId"], out2["request"]["sessionId"],
            "同样 user message 应得 stable sessionId"
        );

        // safetySettings 删了
        assert!(out["request"].get("safetySettings").is_none());

        // toolConfig 搬到 request 子对象
        assert!(out.get("toolConfig").is_none());
        assert_eq!(
            out["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "AUTO"
        );
    }

    /// image 模型用不同的 requestType + requestId 形态,且不加 sessionId
    #[test]
    fn antigravity_transform_image_model_uses_image_gen_request_id() {
        let envelope = json!({
            "model": "gemini-3.1-flash-image",
            "project": "p",
            "user_prompt_id": "u",
            "request": {"contents": [{"role":"user","parts":[{"text":"draw a cat"}]}]}
        });
        let out = apply_antigravity_transform(envelope, "gemini-3.1-flash-image").unwrap();
        assert_eq!(out["requestType"], "image_gen");
        let request_id = out["requestId"].as_str().unwrap();
        assert!(
            request_id.starts_with("image_gen/"),
            "image 模型 requestId 应是 image_gen/<ms>/<uuid>/12,实际 {request_id}"
        );
        assert!(
            request_id.ends_with("/12"),
            "image 模型 requestId 必须以 /12 结尾(CLIProxyAPI 硬编码),实际 {request_id}"
        );
        // image 模型不加 sessionId
        assert!(out["request"].get("sessionId").is_none());
    }

    /// toolConfig 已存在 request 内时 — 顶层 toolConfig 不覆盖
    #[test]
    fn antigravity_transform_does_not_overwrite_existing_request_tool_config() {
        let envelope = json!({
            "model": "gemini-3-pro-low",
            "project": "p",
            "user_prompt_id": "u",
            "request": {
                "contents": [{"role":"user","parts":[{"text":"x"}]}],
                "toolConfig": {"existing": true}
            },
            "toolConfig": {"new": true}
        });
        let out = apply_antigravity_transform(envelope, "gemini-3-pro-low").unwrap();
        // request.toolConfig 保留 existing,顶层 toolConfig 不动(CLIProxyAPI
        // 检查 `request.toolConfig` 不存在才搬)
        assert_eq!(out["request"]["toolConfig"]["existing"], true);
        // 顶层 toolConfig 仍在(因为没搬迁)
        assert_eq!(out["toolConfig"]["new"], true);
    }
}
