//! QoderWork CN 模型目录单一真相(key ↔ 显示名 ↔ 最大 context)。
//!
//! 数据实测自 QoderWork 客户端 server model-list(`main.log` `ModelService get_models`,
//! scene=`qwork`,MOC-297)。`key` = 网关 `model_config.key`(同时是 Codex catalog id / wire
//! model);**网关对未知 key 静默 fallback 到 `auto`,故必须精确**,不能猜。
//!
//! **多处消费(单一来源,防漂移)**:① src-tauri `fetch_provider_models`(获取模型目录);
//! ② desktop catalog(`snapshot.rs`)的 Codex picker `display_name`;③ [`crate::model_context_policy`]
//! 的 context window。思考档在 [`crate::reasoning_tiers`](按 key 分派,数据同源本表 doc)。

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
}

/// QoderWork 模型全表(顺序对齐客户端 picker)。
pub const QODER_MODELS: &[QoderModel] = &[
    QoderModel {
        key: "auto",
        display_name: "Auto",
        max_context: 180_000,
    },
    QoderModel {
        key: "qmodel_latest",
        display_name: "Qwen3.7-Max",
        max_context: 1_000_000,
    },
    QoderModel {
        key: "qmodel",
        display_name: "Qwen3.7-Plus",
        max_context: 1_000_000,
    },
    QoderModel {
        key: "l",
        display_name: "Qwen3.6-Flash",
        max_context: 1_000_000,
    },
    QoderModel {
        key: "dmodel",
        display_name: "DeepSeek-V4-Pro",
        max_context: 1_000_000,
    },
    QoderModel {
        key: "dfmodel",
        display_name: "DeepSeek-V4-Flash",
        max_context: 1_000_000,
    },
    QoderModel {
        key: "gm51model",
        display_name: "GLM-5.2",
        max_context: 1_000_000,
    },
    QoderModel {
        key: "kmodel",
        display_name: "Kimi-K2.7-Code",
        max_context: 256_000,
    },
    QoderModel {
        key: "mmodel",
        display_name: "MiniMax-M2.7",
        max_context: 200_000,
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
}
