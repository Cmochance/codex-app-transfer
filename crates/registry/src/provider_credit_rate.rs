//! [MOC-289] 通用:模型显示名 + Credit 折扣倍率后缀渲染。
//!
//! QoderWork / WorkBuddy 等 provider 都在客户端 model picker 给每个模型标一个固定的
//! Credit 消耗倍率(如 `0.79×`)。本函数是这些 provider 共用的**单一渲染口径**,保证各家
//! 倍率后缀样式一致(中点 ` · ` 分隔 + 全角乘号 `×`);无固定倍率(如智能路由 `Auto`)→
//! 只显示名字、不带后缀。各 provider 的模型目录(`qoder_catalog` / `workbuddy_catalog`)
//! 持有各自的 `{key, display_name, credit_rate}` 数据,渲染统一走这里。

/// 有倍率 → `"GLM-5.2 · 0.79×"`;无倍率(`None`)→ `"Auto"`。
pub fn display_name_with_rate(display_name: &str, credit_rate: Option<&str>) -> String {
    match credit_rate {
        Some(rate) => format!("{display_name} · {rate}×"),
        None => display_name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_midpoint_and_times_or_bare_name() {
        assert_eq!(
            display_name_with_rate("GLM-5.2", Some("0.79")),
            "GLM-5.2 · 0.79×"
        );
        assert_eq!(display_name_with_rate("Auto", None), "Auto");
        assert!(!display_name_with_rate("Auto", None).contains('×'));
    }
}
