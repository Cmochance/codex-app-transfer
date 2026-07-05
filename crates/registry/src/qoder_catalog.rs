//! QoderWork CN 模型目录单一真相(key ↔ 显示名 ↔ 最大 context)。
//!
//! 数据实测自 QoderWork 客户端 server model-list(`main.log` `ModelService get_models`,
//! scene=`qwork`,MOC-297)。`key` = 网关 `model_config.key`(同时是 Codex catalog id / wire
//! model);**网关对未知 key 静默 fallback 到 `auto`,故必须精确**,不能猜。
//!
//! **多处消费(单一来源,防漂移)**:① src-tauri `fetch_provider_models`(获取模型目录);
//! ② desktop catalog(`snapshot.rs`)的 Codex picker `display_name`(带 Credit 倍率后缀);
//! ③ [`crate::model_context_policy`] 的 context window。思考档在 [`crate::reasoning_tiers`]
//! (按 key 分派,数据同源本表 doc)。
//!
//! **Credit 倍率(`credit_rate`)数据源**(优先官方文档,便于后续 GitHub Action 检测更新):
//! 官方 <https://docs.qoder.com/zh/cli/model.md>「前沿模型 Credit 消耗倍率」表(2026-07-05)。
//! 7 个前沿模型有权威官方倍率;`Qwen3.6-Flash` 官方前沿表未单列,取 QoderWork 客户端 picker
//! 实测值;`Auto` 是智能路由分级(倍率随实际路由浮动 ~1.0×,非固定折扣)→ `None`(不显示)。
//! 倍率是**纯展示装饰**、无运行时计费校正:上游改倍率/换 key 时展示会滞后,靠计划中的
//! GitHub Action 定时 diff 官方 model.md 收敛。

/// 一条 QoderWork 模型。
#[derive(Debug, Clone, Copy)]
pub struct QoderModel {
    /// 网关 `model_config.key`(= Codex catalog id / wire model)。
    pub key: &'static str,
    /// Codex model picker 显示名(否则会露原始 key 如 `gm51model`)。
    pub display_name: &'static str,
    /// 该模型支持的**最大** context window(用最大档;QoderWork 多数支持到 1M,见 server
    /// model-list `availableContextWindows`)。
    pub max_context: u64,
    /// Credit 消耗倍率字符串(如 `"0.6"` → picker 显示 `GLM-5.2 · 0.6×`)。`None` = 无固定
    /// 倍率(智能路由 `Auto`),picker 只显示 display_name、不带倍率后缀。数据源见模块 doc。
    pub credit_rate: Option<&'static str>,
}

impl QoderModel {
    /// Codex picker 的显示名:有倍率 → `"GLM-5.2 · 0.6×"`,无 → `"Auto"`。
    /// 中点 ` · ` 分隔,倍率后缀全角乘号 `×`。
    pub fn display_name_with_rate(&self) -> String {
        match self.credit_rate {
            Some(rate) => format!("{} · {rate}×", self.display_name),
            None => self.display_name.to_owned(),
        }
    }
}

/// QoderWork 模型全表(顺序对齐客户端 picker)。
pub const QODER_MODELS: &[QoderModel] = &[
    // 智能路由:倍率随实际路由浮动(官方分级 ~1.0×),非固定折扣 → None(不显示倍率)
    QoderModel {
        key: "auto",
        display_name: "Auto",
        max_context: 180_000,
        credit_rate: None,
    },
    // 官方:Qwen3.7-Max 0.5×
    QoderModel {
        key: "qmodel_latest",
        display_name: "Qwen3.7-Max",
        max_context: 1_000_000,
        credit_rate: Some("0.5"),
    },
    // 官方:Qwen3.7-Plus 0.1×
    QoderModel {
        key: "qmodel",
        display_name: "Qwen3.7-Plus",
        max_context: 1_000_000,
        credit_rate: Some("0.1"),
    },
    // 客户端 picker:Qwen3.6-Flash 0.1×(官方 cli/model 前沿表未单列)
    QoderModel {
        key: "l",
        display_name: "Qwen3.6-Flash",
        max_context: 1_000_000,
        credit_rate: Some("0.1"),
    },
    // 官方:DeepSeek-V4-Pro 0.5×
    QoderModel {
        key: "dmodel",
        display_name: "DeepSeek-V4-Pro",
        max_context: 1_000_000,
        credit_rate: Some("0.5"),
    },
    // 官方:DeepSeek-V4-Flash 0.1×
    QoderModel {
        key: "dfmodel",
        display_name: "DeepSeek-V4-Flash",
        max_context: 1_000_000,
        credit_rate: Some("0.1"),
    },
    // 官方:GLM-5.2 0.6×
    QoderModel {
        key: "gm51model",
        display_name: "GLM-5.2",
        max_context: 1_000_000,
        credit_rate: Some("0.6"),
    },
    // 官方:Kimi-K2.7-Code 0.3×(开 Fast 开关升 0.6×,此处取常规档)
    QoderModel {
        key: "kmodel",
        display_name: "Kimi-K2.7-Code",
        max_context: 256_000,
        credit_rate: Some("0.3"),
    },
    // 官方:MiniMax-M3 0.2×(官方现列 M3;本 key `mmodel` 展示名仍 M2.7,倍率一致)
    QoderModel {
        key: "mmodel",
        display_name: "MiniMax-M2.7",
        max_context: 200_000,
        credit_rate: Some("0.2"),
    },
];

/// 网关 key → Codex picker 显示名(未知返 `None`)。
pub fn qoder_display_name(key: &str) -> Option<&'static str> {
    let k = key.trim();
    QODER_MODELS
        .iter()
        .find(|m| m.key == k)
        .map(|m| m.display_name)
}

/// 网关 key → 最大 context window(未知返 `None`)。
pub fn qoder_max_context(key: &str) -> Option<u64> {
    let k = key.trim();
    QODER_MODELS
        .iter()
        .find(|m| m.key == k)
        .map(|m| m.max_context)
}

/// provider 的 `authScheme` 是否为 QoderWork(`qoder_oauth` 及历史别名)。
///
/// **为什么需要**:QoderWork 的网关 key 里有 `auto` / `l` 这类**通用别名**,与其它 provider
/// (如 WorkBuddy 也用 `auto`)撞名。而 [`crate::reasoning_tiers`] / [`crate::model_context_policy`]
/// 的思考档 + context 表是**按 model id 全局 keying、不看 provider**(MOC-241 假设 model id 全局唯一)。
/// 若把 qoder 的 `auto` 塞进全局表,会静默改掉 WorkBuddy `auto` 的 reasoning/context(跨 provider 回归)。
/// 故 qoder 的 key 只在**本 provider 上下文**里生效:catalog / wire / compact / supports_1m 四处消费点
/// 都用本判据 scope,非 qoder provider 走全局表(qoder key → 无特殊档 = 各自 provider 原行为)。
pub fn is_qoder_auth_scheme(auth_scheme: &str) -> bool {
    matches!(auth_scheme.trim(), "qoder_oauth" | "qoder" | "qoder_cosy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_9_models_lookup_works() {
        assert_eq!(QODER_MODELS.len(), 9);
        assert_eq!(qoder_display_name("gm51model"), Some("GLM-5.2"));
        assert_eq!(qoder_display_name("l"), Some("Qwen3.6-Flash"));
        assert_eq!(qoder_display_name(" auto "), Some("Auto"));
        assert_eq!(qoder_display_name("nope"), None);
        assert_eq!(qoder_max_context("gm51model"), Some(1_000_000));
        assert_eq!(qoder_max_context("kmodel"), Some(256_000));
        assert_eq!(qoder_max_context("mmodel"), Some(200_000));
        assert_eq!(qoder_max_context("nope"), None);
    }

    #[test]
    fn display_name_with_rate_formats_midpoint_and_times() {
        let by = |key: &str| QODER_MODELS.iter().find(|m| m.key == key).unwrap();
        // 有倍率 → "名字 · 倍率×"
        assert_eq!(by("gm51model").display_name_with_rate(), "GLM-5.2 · 0.6×");
        assert_eq!(by("qmodel").display_name_with_rate(), "Qwen3.7-Plus · 0.1×");
        // Auto 无倍率 → 只名字,不带后缀
        assert_eq!(by("auto").display_name_with_rate(), "Auto");
        assert!(!by("auto").display_name_with_rate().contains('×'));
    }

    #[test]
    fn official_rates_pinned_to_docs_qoder_model_table() {
        // 倍率钉在官方 docs.qoder.com/zh/cli/model.md 的 2026-07-05 快照上。这是**本地改动
        // 侦测**(pin):任何人无意改了表里某个倍率,这里立刻炸,逼其确认是否真按官方同步。
        // 上游漂移侦测由计划中的 GitHub Action 负责(见模块 doc),不是本测试。
        let rate = |key: &str| {
            QODER_MODELS
                .iter()
                .find(|m| m.key == key)
                .unwrap()
                .credit_rate
        };
        assert_eq!(rate("qmodel_latest"), Some("0.5")); // Qwen3.7-Max
        assert_eq!(rate("qmodel"), Some("0.1")); // Qwen3.7-Plus
        assert_eq!(rate("l"), Some("0.1")); // Qwen3.6-Flash(客户端源)
        assert_eq!(rate("dmodel"), Some("0.5")); // DeepSeek-V4-Pro
        assert_eq!(rate("dfmodel"), Some("0.1")); // DeepSeek-V4-Flash
        assert_eq!(rate("gm51model"), Some("0.6")); // GLM-5.2
        assert_eq!(rate("kmodel"), Some("0.3")); // Kimi-K2.7-Code(常规档)
        assert_eq!(rate("mmodel"), Some("0.2")); // MiniMax-M3
        assert_eq!(rate("auto"), None); // 智能路由分级,无固定倍率
    }
}
