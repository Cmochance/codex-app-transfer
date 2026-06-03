//! 按 host 决定走 ImpersonatingClient 还是原 reqwest
//!
//! 列表中后缀匹配即需要伪装 (TLS 指纹)。列表外的 host 一律走原 reqwest
//! (`status.openai.com` / `community.openai.com` / `github.com` 等无 CF 强保
//! 的, 伪装只会多耗 CPU 无收益, 但不破坏正确性, 故保守用同列表)。

/// 需要浏览器 TLS 指纹伪装的 host 后缀 (小写)
pub const IMPERSONATE_HOSTS: &[&str] = &["openai.com", "chatgpt.com", "oaistatic.com"];

/// 已知无 CF 强保的子域 (按 host 精确匹配, 大小写不敏感, 走小写比较)。
/// 这些子域虽然后缀匹配 IMPERSONATE_HOSTS, 但实际没强 JS 挑战, 不需要伪装。
pub const IMPERSONATE_EXCLUDE_HOSTS: &[&str] = &["status.openai.com", "community.openai.com"];

/// 判断 host 是否需要伪装。`host` 接受带或不带 `www.` 前缀, 大小写不敏感。
pub fn should_impersonate(host: &str) -> bool {
    let h = host.trim().trim_start_matches("www.").to_ascii_lowercase();
    if IMPERSONATE_EXCLUDE_HOSTS
        .iter()
        .any(|excluded| h == *excluded)
    {
        return false;
    }
    IMPERSONATE_HOSTS
        .iter()
        .any(|suffix| h == *suffix || h.ends_with(&format!(".{suffix}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impersonates_cf_strict_openai_hosts() {
        for host in [
            "chatgpt.com",
            "api.openai.com",
            "help.openai.com",
            "platform.openai.com",
            "openai.com",
            "www.openai.com",
            "persistent.oaistatic.com",
        ] {
            assert!(
                should_impersonate(host),
                "expected impersonation for {host}"
            );
        }
    }

    #[test]
    fn does_not_impersonate_non_openai_hosts() {
        for host in [
            "github.com",
            "api.github.com",
            "google.com",
            "anthropic.com",
            "example.com",
            "127.0.0.1",
        ] {
            assert!(
                !should_impersonate(host),
                "expected NO impersonation for {host}"
            );
        }
    }

    #[test]
    fn handles_case_and_www() {
        assert!(should_impersonate("ChatGPT.com"));
        assert!(should_impersonate("WWW.OpenAI.com"));
        assert!(should_impersonate("  chatgpt.com  "));
    }
}
