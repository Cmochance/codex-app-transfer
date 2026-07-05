//! [MOC-289] WorkBuddy(腾讯 CodeBuddy)模型 Credit 折扣倍率(单一权威表)。
//!
//! WorkBuddy 的每模型固定倍率来自**客户端 product 配置**(`$schema: product-schema.json`,
//! 由 `copilot.tencent.com` / `codebuddy.ai` 下发,app 缓存于
//! `~/.workbuddy/local_storage/entry_*.info`,base64+gzip),`models[].credits` 字段形如
//! `"x0.79"`。**注意**:WorkBuddy 的公开网页积分文档只写「按模型等级动态消耗」,**不含**
//! 每模型倍率表 —— 权威源是客户端下发的 product 配置(与 QoderWork 同理,倍率在客户端
//! model-list、非公开文档)。倍率随实测截图逐条核对(2026-07-05)。
//!
//! 本表只覆盖 **transfer 实际暴露给用户的 10 个模型**(preset `workbuddy` /
//! `workbuddy-login` 的 modelCapabilities),`key` = 网关 model id(= Codex catalog id /
//! wire model),`credit_rate` = product 配置 `credits` 去掉 `x` 前缀。`Auto`(智能路由)
//! 无固定倍率 → `None`(不显示后缀)。
//!
//! **消费点**:desktop snapshot `workbuddy_display_names` 建 `{key: "display_name · 倍率×"}`
//! 覆盖 Codex picker 显示名(否则 WorkBuddy 模型露原始 id 如 `glm-5.2`、且无倍率)。渲染
//! 统一走 [`crate::provider_credit_rate::display_name_with_rate`](与 QoderWork 同口径)。

/// WorkBuddy 一个模型的目录条目(倍率显示用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkbuddyModel {
    /// 网关 model id(= Codex catalog id / wire model)。
    pub key: &'static str,
    /// Codex picker 显示名(否则露原始 id)。
    pub display_name: &'static str,
    /// Credit 消耗倍率字符串(如 `"0.79"` → 显示 `GLM-5.2 · 0.79×`)。`None` = 无固定倍率
    /// (智能路由 `Auto`),只显示名字。数据源见模块 doc。
    pub credit_rate: Option<&'static str>,
}

impl WorkbuddyModel {
    /// Codex picker 显示名:有倍率 → `"GLM-5.2 · 0.79×"`,无 → `"Auto"`。
    pub fn display_name_with_rate(&self) -> String {
        crate::provider_credit_rate::display_name_with_rate(self.display_name, self.credit_rate)
    }
}

/// WorkBuddy 模型目录(仅 transfer 暴露的 10 个;`credit_rate` 取 app product 配置 `credits`)。
///
/// 倍率来源:WorkBuddy app product 配置 `models[].credits`(2026-07-05 实测)。`hy3-preview`
/// 我们路由此 id,product 配置对应的唯一 Hy3 模型是 `hy3-preview-agent`(x0.04),取其倍率。
pub const WORKBUDDY_MODELS: &[WorkbuddyModel] = &[
    // 智能路由:无固定倍率 → 不显示后缀
    WorkbuddyModel {
        key: "auto",
        display_name: "Auto",
        credit_rate: None,
    },
    WorkbuddyModel {
        key: "hy3-preview",
        display_name: "Hy3 preview",
        credit_rate: Some("0.04"),
    },
    WorkbuddyModel {
        key: "deepseek-v4-flash",
        display_name: "Deepseek-V4-Flash",
        credit_rate: Some("0.06"),
    },
    WorkbuddyModel {
        key: "deepseek-v4-pro",
        display_name: "Deepseek-V4-Pro",
        credit_rate: Some("0.16"),
    },
    WorkbuddyModel {
        key: "minimax-m3",
        display_name: "MiniMax-M3",
        credit_rate: Some("0.25"),
    },
    WorkbuddyModel {
        key: "kimi-k2.6",
        display_name: "Kimi-K2.6",
        credit_rate: Some("0.52"),
    },
    WorkbuddyModel {
        key: "kimi-k2.7",
        display_name: "Kimi-K2.7-Code",
        credit_rate: Some("0.57"),
    },
    WorkbuddyModel {
        key: "glm-5.1",
        display_name: "GLM-5.1",
        credit_rate: Some("0.79"),
    },
    WorkbuddyModel {
        key: "glm-5.2",
        display_name: "GLM-5.2",
        credit_rate: Some("0.79"),
    },
    WorkbuddyModel {
        key: "glm-5v-turbo",
        display_name: "GLM-5v-Turbo",
        credit_rate: Some("0.95"),
    },
];

/// provider 的 `authScheme` 是否为 WorkBuddy 账号登录(`workbuddy_oauth` 及别名)。
/// **与 proxy `resolver::AuthScheme::parse` 同 normalize**(trim + lowercase + dash→underscore)。
///
/// 注意:WorkBuddy 的 **API-key preset(`workbuddy`)authScheme 是 `bearer`**(与普通 OpenAI
/// 共用),光凭 authScheme 认不出 —— 那条走 baseUrl host 判定(见 desktop `provider_is_workbuddy`)。
/// 本函数只认账号登录路。
pub fn is_workbuddy_auth_scheme(auth_scheme: &str) -> bool {
    matches!(
        auth_scheme
            .trim()
            .to_ascii_lowercase()
            .replace('-', "_")
            .as_str(),
        "workbuddy_oauth" | "workbuddy_login"
    )
}

/// Codex model catalog 的 display_names 反查表:`{key: "display_name · 倍率×"}`。
/// (registry serde_json 开 `preserve_order`,map 保持 `WORKBUDDY_MODELS` 顺序。)
pub fn workbuddy_catalog_display_names() -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for m in WORKBUDDY_MODELS {
        map.insert(
            m.key.to_owned(),
            serde_json::Value::String(m.display_name_with_rate()),
        );
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_ten_exposed_models() {
        // transfer 暴露的 10 个模型(preset modelCapabilities),顺序按倍率升序 + Auto 首位。
        assert_eq!(WORKBUDDY_MODELS.len(), 10);
        let keys: Vec<&str> = WORKBUDDY_MODELS.iter().map(|m| m.key).collect();
        for k in [
            "auto",
            "deepseek-v4-pro",
            "deepseek-v4-flash",
            "minimax-m3",
            "glm-5.2",
            "glm-5.1",
            "glm-5v-turbo",
            "kimi-k2.7",
            "kimi-k2.6",
            "hy3-preview",
        ] {
            assert!(keys.contains(&k), "缺模型 {k}");
        }
    }

    #[test]
    fn rates_match_workbuddy_product_config() {
        // 倍率钉在 WorkBuddy app product 配置 `models[].credits`(2026-07-05 实测截图核对)。
        let rate = |key: &str| {
            WORKBUDDY_MODELS
                .iter()
                .find(|m| m.key == key)
                .unwrap()
                .credit_rate
        };
        assert_eq!(rate("hy3-preview"), Some("0.04"));
        assert_eq!(rate("deepseek-v4-flash"), Some("0.06"));
        assert_eq!(rate("deepseek-v4-pro"), Some("0.16"));
        assert_eq!(rate("minimax-m3"), Some("0.25"));
        assert_eq!(rate("kimi-k2.6"), Some("0.52"));
        assert_eq!(rate("kimi-k2.7"), Some("0.57"));
        assert_eq!(rate("glm-5.1"), Some("0.79"));
        assert_eq!(rate("glm-5.2"), Some("0.79"));
        assert_eq!(rate("glm-5v-turbo"), Some("0.95"));
        assert_eq!(rate("auto"), None); // 智能路由无固定倍率
    }

    #[test]
    fn is_workbuddy_auth_scheme_normalizes() {
        for s in [
            "workbuddy_oauth",
            "workbuddy_login",
            "Workbuddy-OAuth",
            " workbuddy_login ",
        ] {
            assert!(is_workbuddy_auth_scheme(s), "应认: {s:?}");
        }
        // bearer(API-key preset)不由 authScheme 认,走 host 判定
        for s in ["bearer", "qoder_oauth", "", "workbuddy"] {
            assert!(!is_workbuddy_auth_scheme(s), "不应认: {s:?}");
        }
    }

    #[test]
    fn display_names_carry_rate_suffix() {
        let names = workbuddy_catalog_display_names();
        assert_eq!(names["glm-5.2"], "GLM-5.2 · 0.79×");
        assert_eq!(names["deepseek-v4-pro"], "Deepseek-V4-Pro · 0.16×");
        assert_eq!(names["auto"], "Auto");
        assert_eq!(names.as_object().unwrap().len(), 10);
    }
}
