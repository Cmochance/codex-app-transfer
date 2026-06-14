//! Provider-neutral 额度类型(MOC-211)。
//!
//! 各 provider 的额度**体系不同**,是相互独立的显示系统:
//! - antigravity(gemini)/ GLM Coding:**5 小时 + 每周**双滚动窗口;
//! - 小米 MiMo Token Plan:**月度套餐**单窗口(自然月重置)。
//!
//! 故中性类型用「带标签的窗口列表」承载:每个 provider 的 fetcher 产出自己的窗口(各带
//! `label`),[`crate::codex_quota_injector`] 按 label 逐条渲染成 bar——5h/周 与 月度套餐
//! 各显各的、互不混淆。新增 provider 额度源只要产出 [`QuotaWindow`] 列表即可,不动渲染层。
//!
//! 口径统一为「剩余」:antigravity 原生返剩余 fraction;GLM/MiMo 原生返已用 percentage,
//! 各自 parser 换算成 `100 - 已用` 落进 `remaining_percent`。

/// 单个额度窗口。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    /// 窗口名(渲染成 bar 的标签):如「5 小时额度」「每周额度」「套餐用量(月)」。
    pub label: String,
    /// **剩余**百分比(满额=100,消耗后降),clamp 0-100。
    pub remaining_percent: f64,
    /// 窗口重置时刻(RFC3339;各 parser 把自家时间格式统一转 RFC3339)。None=不显刷新时间。
    pub reset_rfc3339: Option<String>,
}

/// 一个 provider 的额度(按渲染顺序的窗口列表)。空 = 不显额度行。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderQuota {
    pub windows: Vec<QuotaWindow>,
}

impl ProviderQuota {
    /// 是否有任一窗口(caller 判定要不要显示额度行)。
    pub fn has_any(&self) -> bool {
        !self.windows.is_empty()
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
        Self { windows }
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
        assert_eq!(p.windows[0].label, "5 小时额度");
        assert_eq!(p.windows[0].remaining_percent, 98.0);
        assert_eq!(p.windows[0].reset_rfc3339.as_deref(), Some("2026-06-13T17:56:06Z"));
        assert_eq!(p.windows[1].label, "每周额度");
    }

    #[test]
    fn empty_has_no_window() {
        assert!(!ProviderQuota::default().has_any());
    }
}
