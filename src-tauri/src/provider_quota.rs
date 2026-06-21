//! Provider-neutral 额度类型(MOC-211 / CAT-256 统一三槽)。
//!
//! 各 coding-plan provider 的滚动窗口额度**统一成 5 小时 / 每周 / 每月 三槽**([`RollingWindows`]),
//! 每档 `Option` —— 有则填、无则 `None`(渲染时不显示该档),所以同一套结构涵盖:
//! - GLM Coding / antigravity:`{5h, 周}`(月 = None);
//! - OpenCode Go:`{5h, 周, 月}`(三档全有);
//! - 小米 MiMo Token Plan:`{月}`(5h/周 = None;月槽文案为「套餐用量」,见 `monthly_labeled`)。
//!
//! 另有 [`QuotaStat`] 纯数值条目(无 5h/周/月 滚动语义,**不进三槽**):如 DeepSeek「余额 ¥5.37」
//! → 渲染成 `label: value` 文本条。
//!
//! [`crate::codex_quota_injector`] 按 `5h→周→月` 固定顺序逐条渲染存在的窗口 + stats;各 provider
//! 各显各的、互不混淆。新增同类 provider 只要产出 [`RollingWindows`](builder 链)即可。

/// 单个额度窗口(有满额/剩余语义 → 进度条)。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    /// 窗口名(bar 标签):如「5 小时额度」「每周额度」「每月额度」「套餐用量」。
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

/// 统一的「5 小时 / 每周 / 每月」三槽滚动窗口。每档 `Option`,`None` = 该 provider 无此档、不显示。
/// 用 builder 链填(标签由槽位固定,避免各 provider 文案漂移):
/// `RollingWindows::default().five_hour(96.0, reset).weekly(99.0, None)`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RollingWindows {
    pub five_hour: Option<QuotaWindow>,
    pub weekly: Option<QuotaWindow>,
    pub monthly: Option<QuotaWindow>,
}

impl RollingWindows {
    /// 5 小时窗口(标签固定「5 小时额度」)。`remaining_percent` 自动 clamp 0-100。
    pub fn five_hour(mut self, remaining_percent: f64, reset_rfc3339: Option<String>) -> Self {
        self.five_hour = Some(make_window("5 小时额度", remaining_percent, reset_rfc3339));
        self
    }
    /// 每周窗口(标签固定「每周额度」)。
    pub fn weekly(mut self, remaining_percent: f64, reset_rfc3339: Option<String>) -> Self {
        self.weekly = Some(make_window("每周额度", remaining_percent, reset_rfc3339));
        self
    }
    /// 每月窗口(标签固定「每月额度」)。
    pub fn monthly(self, remaining_percent: f64, reset_rfc3339: Option<String>) -> Self {
        self.monthly_labeled("每月额度", remaining_percent, reset_rfc3339)
    }
    /// 月槽自定义文案 —— 给「月度套餐」类(MiMo「套餐用量」:按 plan period 重置,非日历月额度,
    /// 结构上归月槽但文案保留各自准确语义)。
    pub fn monthly_labeled(
        mut self,
        label: impl Into<String>,
        remaining_percent: f64,
        reset_rfc3339: Option<String>,
    ) -> Self {
        self.monthly = Some(make_window(label, remaining_percent, reset_rfc3339));
        self
    }
    /// 按 `5h→周→月` 固定顺序遍历**存在**的窗口(`None` 跳过)。渲染只认这个顺序。
    pub fn iter(&self) -> impl Iterator<Item = &QuotaWindow> {
        [&self.five_hour, &self.weekly, &self.monthly]
            .into_iter()
            .flatten()
    }
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.weekly.is_none() && self.monthly.is_none()
    }
}

fn make_window(
    label: impl Into<String>,
    remaining_percent: f64,
    reset: Option<String>,
) -> QuotaWindow {
    QuotaWindow {
        label: label.into(),
        remaining_percent: remaining_percent.clamp(0.0, 100.0),
        reset_rfc3339: reset,
    }
}

/// 一个 provider 的额度(三槽滚动窗口 + 数值条目)。两者皆空 = 不显额度行。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderQuota {
    pub rolling: RollingWindows,
    pub stats: Vec<QuotaStat>,
}

impl ProviderQuota {
    /// 是否有任一窗口/条目(caller 判定要不要显示额度行)。
    pub fn has_any(&self) -> bool {
        !self.rolling.is_empty() || !self.stats.is_empty()
    }
}

// antigravity 的 gemini 双窗口 → 三槽(5h + 周;月 = None)。
impl From<codex_app_transfer_gemini_oauth::GeminiQuota> for ProviderQuota {
    fn from(g: codex_app_transfer_gemini_oauth::GeminiQuota) -> Self {
        let mut rolling = RollingWindows::default();
        if let Some(w) = g.five_hour {
            rolling = rolling.five_hour(w.remaining_percent, w.reset_rfc3339);
        }
        if let Some(w) = g.weekly {
            rolling = rolling.weekly(w.remaining_percent, w.reset_rfc3339);
        }
        Self {
            rolling,
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
        let windows: Vec<&QuotaWindow> = p.rolling.iter().collect();
        assert_eq!(windows.len(), 2);
        assert!(p.stats.is_empty());
        assert_eq!(windows[0].label, "5 小时额度");
        assert_eq!(windows[0].remaining_percent, 98.0);
        assert_eq!(
            windows[0].reset_rfc3339.as_deref(),
            Some("2026-06-13T17:56:06Z")
        );
        assert_eq!(windows[1].label, "每周额度");
        assert!(p.rolling.monthly.is_none());
    }

    #[test]
    fn iter_order_is_5h_weekly_monthly() {
        let r = RollingWindows::default()
            .monthly(100.0, None)
            .five_hour(96.0, None)
            .weekly(99.0, None);
        let labels: Vec<&str> = r.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["5 小时额度", "每周额度", "每月额度"]);
    }

    #[test]
    fn monthly_labeled_keeps_custom_text_and_clamps() {
        let r = RollingWindows::default().monthly_labeled("套餐用量", 150.0, None);
        assert_eq!(r.monthly.as_ref().unwrap().label, "套餐用量");
        assert_eq!(r.monthly.as_ref().unwrap().remaining_percent, 100.0);
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
        assert!(p.rolling.is_empty());
    }
}
