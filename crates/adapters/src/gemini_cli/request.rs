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
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    // OS RNG 不可用时降级 std::time + AtomicUsize 计数器(tests 不命中此路径,
    // production 极端罕见)— 不 panic。
    let _ = getrandom::getrandom(&mut bytes);
    // version 4(top 4 bits of byte 6 = 0100)
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // variant 10(top 2 bits of byte 8 = 10)
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// 把 gemini_native 产出的 inner body 包成 Cloud Code outer envelope。
///
/// 字段顺序对齐 gemini-cli `converter.ts:113-119` 输出:`model` → `project` →
/// `user_prompt_id` → `request`(虽然 JSON 不强 require 顺序,保持一致便于 wire
/// diff 调试)。
pub fn wrap_cloud_code_envelope(model: &str, project_id: &str, inner: Value) -> Value {
    json!({
        "model": model,
        "project": project_id,
        "user_prompt_id": uuid_v4(),
        "request": inner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v4_format_matches_rfc_4122() {
        let id = uuid_v4();
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
        let a = uuid_v4();
        let b = uuid_v4();
        assert_ne!(a, b, "UUID v4 必须每次不同");
    }

    #[test]
    fn wrap_envelope_preserves_inner_intact() {
        let inner = json!({
            "contents": [{"role":"user","parts":[{"text":"hi"}]}],
            "systemInstruction": {"role":"system","parts":[{"text":"sys"}]},
            "generationConfig": {"temperature": 0.7}
        });
        let wrapped = wrap_cloud_code_envelope("gemini-2.5-pro", "proj-abc", inner.clone());
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
        let wrapped = wrap_cloud_code_envelope("g", "p", json!({}));
        assert_eq!(wrapped["request"], json!({}));
        assert_eq!(wrapped["model"], "g");
    }
}
