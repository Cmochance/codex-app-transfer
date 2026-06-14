//! Provider-neutral 额度类型(MOC-211)。
//!
//! antigravity(gemini `retrieveUserQuotaSummary`)与 GLM Coding(bigmodel/z.ai
//! `monitor/usage/quota/limit`)的额度形态本质一致:一个 5 小时窗口 + 一个每周窗口,
//! 每窗口有「剩余百分比 + 重置时刻」。各 provider 的 fetcher 把自己的原始响应 `into()`
//! 这套统一形态,[`crate::codex_quota_injector`] 的渲染/payload 层只认这个,新增 provider
//! 额度源时不必再动渲染代码。
//!
//! 各 provider 口径差异在各自 parser 里抹平后统一成「剩余」:antigravity 原生返
//! `remainingFraction`(剩余);GLM 原生返 `percentage`(已用),parser 换算成 `100 - 已用`。

/// 单个额度窗口(5h 或 weekly)。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    /// **剩余**百分比(满额=100,消耗后降),clamp 0-100。
    pub remaining_percent: f64,
    /// 窗口重置时刻(RFC3339;GLM 的 unix-ms 在 parser 转成 RFC3339)。caller 转本地显示。
    pub reset_rfc3339: Option<String>,
}

/// 一个 provider 的双窗口额度。任一窗口缺失(上游没返)→ None。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderQuota {
    pub five_hour: Option<QuotaWindow>,
    pub weekly: Option<QuotaWindow>,
}

impl ProviderQuota {
    /// 是否有任一窗口(caller 判定要不要显示额度行)。
    pub fn has_any(&self) -> bool {
        self.five_hour.is_some() || self.weekly.is_some()
    }
}

// antigravity 的 gemini 额度 → 中性类型(字段语义完全一致,直接搬)。
impl From<codex_app_transfer_gemini_oauth::QuotaWindow> for QuotaWindow {
    fn from(w: codex_app_transfer_gemini_oauth::QuotaWindow) -> Self {
        Self {
            remaining_percent: w.remaining_percent,
            reset_rfc3339: w.reset_rfc3339,
        }
    }
}

impl From<codex_app_transfer_gemini_oauth::GeminiQuota> for ProviderQuota {
    fn from(g: codex_app_transfer_gemini_oauth::GeminiQuota) -> Self {
        Self {
            five_hour: g.five_hour.map(Into::into),
            weekly: g.weekly.map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_gemini_quota_preserves_fields() {
        let g = codex_app_transfer_gemini_oauth::GeminiQuota {
            five_hour: Some(codex_app_transfer_gemini_oauth::QuotaWindow {
                remaining_percent: 98.0,
                reset_rfc3339: Some("2026-06-13T17:56:06Z".into()),
            }),
            weekly: None,
        };
        let p = ProviderQuota::from(g);
        assert!(p.has_any());
        assert!(p.weekly.is_none());
        let h = p.five_hour.expect("5h");
        assert_eq!(h.remaining_percent, 98.0);
        assert_eq!(h.reset_rfc3339.as_deref(), Some("2026-06-13T17:56:06Z"));
    }

    #[test]
    fn empty_has_no_window() {
        assert!(!ProviderQuota::default().has_any());
    }
}
