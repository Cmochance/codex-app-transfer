//! Provider-neutral 额度类型(MOC-211)。
//!
//! 各 provider 的额度**体系不同**,是相互独立的显示系统:
//! - antigravity(gemini)/ GLM Coding:**5 小时 + 每周**双滚动窗口(剩余% 进度条);
//! - 小米 MiMo Token Plan:**月度套餐**单窗口(剩余% 进度条,自然月重置);
//! - DeepSeek:**充值余额**(钱,无上限/百分比)→ 纯数值条目,不画进度条。
//!
//! 故中性类型同时承载两类展示:
//! - [`QuotaWindow`] 列表 → 渲染成带进度条的 bar(有「满额/剩余」语义的);
//! - [`QuotaStat`] 列表 → 渲染成 `label: value` 纯文本条目(余额这种无百分比的)。
//!
//! 每个 provider 的 fetcher 产出自己的窗口/条目(各带 label),[`crate::codex_quota_injector`]
//! 逐条渲染——各 provider 各显各的、互不混淆。新增 provider 额度源只要产出这两个列表即可。

/// 单个额度窗口(有满额/剩余语义 → 进度条)。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    /// 窗口名(bar 标签):如「5 小时额度」「每周额度」「套餐用量」。
    pub label: String,
    /// **剩余**百分比(满额=100,消耗后降),clamp 0-100。
    pub remaining_percent: f64,
    /// 重置时刻(RFC3339;各 parser 把自家时间格式统一转 RFC3339)。None=不显刷新时间。
    pub reset_rfc3339: Option<String>,
}

/// 纯数值条目(无百分比语义 → 不画进度条):如「余额 ¥5.37」。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaStat {
    pub label: String,
    /// 已格式化好的展示值(含币种符号 / 单位),如 `¥5.37`。
    pub value: String,
}

/// 一个 provider 的额度(窗口 bar + 数值条目)。两者皆空 = 不显额度行。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderQuota {
    pub windows: Vec<QuotaWindow>,
    pub stats: Vec<QuotaStat>,
}

impl ProviderQuota {
    /// 是否有任一窗口/条目(caller 判定要不要显示额度行)。
    pub fn has_any(&self) -> bool {
        !self.windows.is_empty() || !self.stats.is_empty()
    }
}

// antigravity 的 gemini 双窗口 → 中性窗口列表(labels:5 小时额度 / 每周额度)。
impl From<codex_app_transfer_gemini_oauth::GeminiQuota> for ProviderQuota {
    fn from(g: codex_app_transfer_gemini_oauth::GeminiQuota) -> Self {
        let mut windows = Vec::new();
        if let Some(w) = g.five_hour {
            windows.push(QuotaWindow {
                label: "5 小时额度".into(),
                remaining_percent: w.remaining_percent,
                reset_rfc3339: w.reset_rfc3339,
            });
        }
        if let Some(w) = g.weekly {
            windows.push(QuotaWindow {
                label: "每周额度".into(),
                remaining_percent: w.remaining_percent,
                reset_rfc3339: w.reset_rfc3339,
            });
        }
        Self {
            windows,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_gemini_quota_maps_both_windows_in_order() {
        let g = codex_app_transfer_gemini_oauth::GeminiQuota {
            five_hour: Some(codex_app_transfer_gemini_oauth::QuotaWindow {
                remaining_percent: 98.0,
                reset_rfc3339: Some("2026-06-13T17:56:06Z".into()),
            }),
            weekly: Some(codex_app_transfer_gemini_oauth::QuotaWindow {
                remaining_percent: 100.0,
                reset_rfc3339: None,
            }),
        };
        let p = ProviderQuota::from(g);
        assert!(p.has_any());
        assert_eq!(p.windows.len(), 2);
        assert!(p.stats.is_empty());
        assert_eq!(p.windows[0].label, "5 小时额度");
        assert_eq!(p.windows[0].remaining_percent, 98.0);
        assert_eq!(p.windows[0].reset_rfc3339.as_deref(), Some("2026-06-13T17:56:06Z"));
        assert_eq!(p.windows[1].label, "每周额度");
    }

    #[test]
    fn empty_has_no_window() {
        assert!(!ProviderQuota::default().has_any());
    }

    #[test]
    fn stats_only_counts_as_has_any() {
        let p = ProviderQuota {
            stats: vec![QuotaStat {
                label: "余额".into(),
                value: "¥5.37".into(),
            }],
            ..Default::default()
        };
        assert!(p.has_any());
        assert!(p.windows.is_empty());
    }
}
