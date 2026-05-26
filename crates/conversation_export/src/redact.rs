//! 简单 regex-free secret redaction(避免引 regex crate 增加体积)。
//!
//! 处理 4 类常见密钥 pattern:
//! - OpenAI / Anthropic 等 `sk-` 前缀:`sk-[A-Za-z0-9_-]{12,}`
//! - codex-app-transfer gateway key:`cas_[A-Za-z0-9_-]{12,}`
//! - JWT 三段式:`[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}`
//! - `Authorization: Bearer <token>` header
//!
//! 替换为 `[REDACTED]`。

use std::borrow::Cow;

/// 替换文本里的密钥 pattern → `[REDACTED]`。无命中时 borrow,无内存拷贝。
pub fn redact_secrets(input: &str) -> Cow<'_, str> {
    let mut s = Cow::Borrowed(input);
    s = redact_prefix(s, "sk-", 12);
    s = redact_prefix(s, "cas_", 12);
    s = redact_bearer(s);
    s = redact_jwt(s);
    s
}

fn redact_prefix<'a>(input: Cow<'a, str>, prefix: &str, min_tail: usize) -> Cow<'a, str> {
    if !input.contains(prefix) {
        return input;
    }
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if input[i..].starts_with(prefix) {
            let start = i;
            let mut j = i + prefix.len();
            while j < bytes.len() {
                let b = bytes[j];
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                    j += 1;
                } else {
                    break;
                }
            }
            if j - start - prefix.len() >= min_tail {
                out.push_str("[REDACTED]");
                i = j;
                continue;
            }
        }
        // 推一个字符过去(走 char 边界,UTF-8 安全)
        let ch_end = next_char_boundary(&input, i);
        out.push_str(&input[i..ch_end]);
        i = ch_end;
    }
    Cow::Owned(out)
}

fn redact_bearer<'a>(input: Cow<'a, str>) -> Cow<'a, str> {
    // 命中后把 token 部分换为 [REDACTED]
    let needle = b"bearer ";
    // devin #272 review fix:用字节级 case-insensitive 扫描判 prefix 存在
    // (避免 `to_ascii_lowercase()` 给每条 export 文本无脑 alloc 一份小写
    // 副本 — 99% 的文本不含 "bearer ")
    let input_bytes = input.as_bytes();
    let has_bearer = input_bytes
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle));
    if !has_bearer {
        return input;
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    // 同 redact_jwt:仅当真有 redaction 才返 Cow::Owned;含 "bearer " 但
    // 后续 token 不足 8 char(如 "use bearer trust...")时回 borrow。
    let mut did_redact = false;
    while i < input_bytes.len() {
        // 找下一个 "bearer "(case-insensitive)起点
        let next_match = input_bytes[i..]
            .windows(needle.len())
            .position(|w| w.eq_ignore_ascii_case(needle))
            .map(|p| i + p);
        let Some(abs) = next_match else {
            out.push_str(&input[i..]);
            break;
        };
        out.push_str(&input[i..abs + needle.len()]);
        let mut j = abs + needle.len();
        while j < input_bytes.len() {
            let b = input_bytes[j];
            if b.is_ascii_alphanumeric()
                || b == b'.'
                || b == b'_'
                || b == b'-'
                || b == b'+'
                || b == b'/'
                || b == b'='
            {
                j += 1;
            } else {
                break;
            }
        }
        if j - abs - needle.len() >= 8 {
            out.push_str("[REDACTED]");
            did_redact = true;
        } else {
            out.push_str(&input[abs + needle.len()..j]);
        }
        i = j;
    }
    if did_redact {
        Cow::Owned(out)
    } else {
        input
    }
}

fn redact_jwt<'a>(input: Cow<'a, str>) -> Cow<'a, str> {
    if !input.contains('.') {
        return input;
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    // devin #272 review fix:跟踪是否真有 redaction 发生 — 否则任何含 `.`
    // 的自然文本(几乎所有句子 / URL / 文件名)都会被无谓 alloc + rebuild
    let mut did_redact = false;
    while i < bytes.len() {
        // 找一段连续 [A-Za-z0-9_-]{8,}
        let start = i;
        while i < bytes.len() && is_jwt_seg_byte(bytes[i]) {
            i += 1;
        }
        if i - start >= 8 && i < bytes.len() && bytes[i] == b'.' {
            // 第一段长度 ≥ 8,尝试 segment 2
            let s2 = i + 1;
            let mut k = s2;
            while k < bytes.len() && is_jwt_seg_byte(bytes[k]) {
                k += 1;
            }
            if k - s2 >= 8 && k < bytes.len() && bytes[k] == b'.' {
                let s3 = k + 1;
                let mut m = s3;
                while m < bytes.len() && is_jwt_seg_byte(bytes[m]) {
                    m += 1;
                }
                if m - s3 >= 8 {
                    out.push_str("[REDACTED]");
                    i = m;
                    did_redact = true;
                    continue;
                }
            }
        }
        // 没匹到完整 jwt → 写回原 segment + 当前字节
        out.push_str(&input[start..i]);
        if i < bytes.len() {
            let ch_end = next_char_boundary(&input, i);
            out.push_str(&input[i..ch_end]);
            i = ch_end;
        }
    }
    if did_redact {
        Cow::Owned(out)
    } else {
        input
    }
}

fn is_jwt_seg_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn next_char_boundary(s: &str, mut i: usize) -> usize {
    i += 1;
    while !s.is_char_boundary(i) && i < s.len() {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_sk_keys() {
        let out = redact_secrets("OpenAI key: sk-proj-1234567890ABCDEF tail");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("sk-proj-1234567890ABCDEF"));
    }

    #[test]
    fn redacts_cas_gateway_keys() {
        let out = redact_secrets("export CAS=cas_abcd1234efgh5678");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("cas_abcd1234efgh5678"));
    }

    #[test]
    fn redacts_bearer_tokens() {
        let out = redact_secrets("Authorization: Bearer eyJabcd1234EFGH=");
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("eyJabcd1234"));
    }

    #[test]
    fn redacts_jwt_three_segment() {
        // 经典 JWT 三段 base64url
        let jwt =
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NSJ9.abcdefghijklmnop12345678";
        let input = format!("token={jwt} ok");
        let out = redact_secrets(&input);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains(jwt));
    }

    #[test]
    fn keeps_normal_text_unchanged() {
        let input = "Hello world, my user is alice and email is alice@example.com.";
        let out = redact_secrets(input);
        assert_eq!(out.as_ref(), input);
    }

    /// **devin #272 review 防回归**:zero-copy 文档承诺 — 自然文本(含
    /// dot / 大小写混合 "Bearer Trust 关键字" 等常见模式但无真签名)
    /// 必须返 `Cow::Borrowed`,不能 alloc。
    #[test]
    fn returns_borrowed_for_text_without_real_secrets() {
        let input =
            "Visit https://example.com/path.html for the docs. Use bearer trust in your team.";
        let out = redact_secrets(input);
        assert!(
            matches!(out, std::borrow::Cow::Borrowed(_)),
            "含 dot / 小写 bearer 关键字但无真 token,必须 borrow 不 alloc"
        );
        assert_eq!(out.as_ref(), input);
    }

    #[test]
    fn does_not_redact_short_prefix_matches() {
        // sk-1 太短,不该被当 key
        let input = "sk-1 is too short";
        let out = redact_secrets(input);
        assert_eq!(out.as_ref(), input);
    }

    #[test]
    fn handles_chinese_text_around_secret() {
        let out = redact_secrets("用户密钥是 sk-abc123def456789012 不要外传");
        assert!(out.contains("[REDACTED]"));
        assert!(out.contains("用户密钥是"));
        assert!(out.contains("不要外传"));
    }
}
