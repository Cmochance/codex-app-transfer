//! 配置自愈:对 `isBuiltin=true` 但 `extra_headers` 字段缺失/为空的 provider,
//! 从 `builtin_presets()` 同步补齐。
//!
//! ## 历史背景(为什么需要这个)
//!
//! v2.0.10 实测痛点:用户 Windows 上接 Kimi For Coding 一律 403
//! `access_terminated_error`,macOS 同 API key 正常。根因调查发现 Windows 上
//! `~/.codex-app-transfer/config.json` 里 Kimi Code provider 的 `extraHeaders`
//! 是 `{}` 空对象 —— 运行时 `forward` 不注入 `User-Agent: KimiCLI/1.40.0`,
//! Codex CLI 客户端的 `codex_cli_rs/...` UA 直接透传到 Kimi → 反爬识别非白名单
//! client → 403。
//!
//! 为什么 macOS 没事:用户在 macOS 上**编辑过**这条 Kimi(改过任意字段),
//! 触发 frontend `presetMatchesProvider` 命中 → preset 的 `extraHeaders` 被
//! 同步进 config.json。Windows 上从未编辑过,`extraHeaders` 一直是 `{}`。
//!
//! 真正的根因是**老版本(v1.x)写入的 builtin provider 没有 extras 字段,
//! 升级到新版后既不会自动补齐,也没有 UI 提示**。本模块在每次 load config
//! 时做一次 healing pass,只在内存中补齐 extras,不写回磁盘(让用户的
//! config.json 保持原样,避免跟 import/export 流程冲突)。

use std::collections::HashMap;

use serde_json::Value;

use crate::presets::builtin_presets;

/// 对 `isBuiltin=true` 且 `extraHeaders` 缺失/空的 provider,从 builtin preset
/// 同步补齐。**只在内存中改,不写回磁盘**。
///
/// 触发条件(三者全部满足):
/// 1. provider 的 `isBuiltin` 字段是 `true`
/// 2. provider 的 `extraHeaders` 字段缺失,或值是空对象 `{}`
/// 3. 同 id 的 builtin preset 里 `extraHeaders` 不为空
///
/// 用户已经自定义(extras 非空)的 provider **完全不动**,保留用户意图。
pub fn heal_builtin_extra_headers(cfg: &mut Value) {
    // 一次性收集所有 builtin preset 的 (id → extraHeaders),按 id O(1) 查找
    let presets = builtin_presets();
    let preset_extras_by_id: HashMap<String, Value> = presets
        .iter()
        .filter_map(|p| {
            let id = p.get("id")?.as_str()?.to_owned();
            let extras = p.get("extraHeaders")?;
            // 只收集 extras 非空的 preset(空就没有补齐价值)
            if extras.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                Some((id, extras.clone()))
            } else {
                None
            }
        })
        .collect();
    if preset_extras_by_id.is_empty() {
        return;
    }

    let Some(providers) = cfg.get_mut("providers").and_then(|v| v.as_array_mut()) else {
        return;
    };

    for provider in providers.iter_mut() {
        let Some(obj) = provider.as_object_mut() else {
            continue;
        };
        // 1) 只处理 isBuiltin=true(用户自建 provider 的 extras 是用户责任)
        let is_builtin = obj
            .get("isBuiltin")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !is_builtin {
            continue;
        }
        // 2) 只在 extraHeaders 缺失或空对象时补
        let needs_heal = obj
            .get("extraHeaders")
            .and_then(|v| v.as_object())
            .map(|o| o.is_empty())
            .unwrap_or(true); // 字段缺失 / 类型不是 object → 也算 needs_heal
        if !needs_heal {
            continue;
        }
        // 3) 同 id preset 里 extras 非空才补
        let Some(id) = obj.get("id").and_then(|v| v.as_str()).map(|s| s.to_owned()) else {
            continue;
        };
        let Some(preset_extras) = preset_extras_by_id.get(&id) else {
            continue;
        };
        obj.insert("extraHeaders".into(), preset_extras.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fills_empty_extras_for_builtin_kimi_code() {
        let mut cfg = json!({
            "providers": [
                {
                    "id": "kimi-code",
                    "name": "Kimi Code",
                    "baseUrl": "https://api.kimi.com/coding/v1",
                    "isBuiltin": true,
                    "extraHeaders": {}
                }
            ]
        });
        heal_builtin_extra_headers(&mut cfg);
        let extras = &cfg["providers"][0]["extraHeaders"];
        assert!(
            extras.as_object().map(|o| !o.is_empty()).unwrap_or(false),
            "Kimi Code 的 extraHeaders 应被自动补齐,实际: {extras}"
        );
        assert_eq!(
            extras["User-Agent"], "KimiCLI/1.40.0",
            "应补 KimiCLI 的 User-Agent"
        );
    }

    #[test]
    fn fills_missing_extras_field_for_builtin() {
        let mut cfg = json!({
            "providers": [
                {
                    "id": "kimi-code",
                    "name": "Kimi Code",
                    "baseUrl": "https://api.kimi.com/coding/v1",
                    "isBuiltin": true
                }
            ]
        });
        heal_builtin_extra_headers(&mut cfg);
        assert!(
            cfg["providers"][0]
                .get("extraHeaders")
                .and_then(|v| v.as_object())
                .map(|o| !o.is_empty())
                .unwrap_or(false),
            "extraHeaders 字段缺失也应被补齐"
        );
    }

    #[test]
    fn does_not_overwrite_user_customized_extras() {
        let user_value = "MyCustomAgent/2.0";
        let mut cfg = json!({
            "providers": [
                {
                    "id": "kimi-code",
                    "isBuiltin": true,
                    "extraHeaders": { "User-Agent": user_value }
                }
            ]
        });
        heal_builtin_extra_headers(&mut cfg);
        assert_eq!(
            cfg["providers"][0]["extraHeaders"]["User-Agent"], user_value,
            "用户自定义的 extras **绝不**被覆盖,即使 preset 里有不同的值"
        );
    }

    #[test]
    fn does_not_touch_non_builtin_providers() {
        let mut cfg = json!({
            "providers": [
                {
                    "id": "kimi-code", // 同 id 但 isBuiltin=false → 当作用户自建,不动
                    "isBuiltin": false,
                    "extraHeaders": {}
                }
            ]
        });
        heal_builtin_extra_headers(&mut cfg);
        assert!(
            cfg["providers"][0]["extraHeaders"]
                .as_object()
                .unwrap()
                .is_empty(),
            "非 builtin provider 的空 extras 不应被自动填充"
        );
    }

    #[test]
    fn no_op_when_preset_id_not_in_builtin_list() {
        let mut cfg = json!({
            "providers": [
                {
                    "id": "totally-unknown-id",
                    "isBuiltin": true,
                    "extraHeaders": {}
                }
            ]
        });
        heal_builtin_extra_headers(&mut cfg);
        assert!(
            cfg["providers"][0]["extraHeaders"]
                .as_object()
                .unwrap()
                .is_empty(),
            "id 不在 builtin presets 里时,什么都不做"
        );
    }

    #[test]
    fn handles_missing_providers_array_gracefully() {
        let mut cfg = json!({"version": "1.0.4"});
        heal_builtin_extra_headers(&mut cfg);
        assert!(
            cfg.get("providers").is_none(),
            "不应额外创建 providers 字段"
        );
    }

    #[test]
    fn heals_multiple_providers_in_one_pass() {
        let mut cfg = json!({
            "providers": [
                {"id": "kimi-code", "isBuiltin": true, "extraHeaders": {}},
                {"id": "user-custom", "isBuiltin": false, "extraHeaders": {}},
                {"id": "kimi-code", "isBuiltin": true} // 字段都缺
            ]
        });
        heal_builtin_extra_headers(&mut cfg);
        assert!(!cfg["providers"][0]["extraHeaders"]
            .as_object()
            .unwrap()
            .is_empty());
        assert!(cfg["providers"][1]["extraHeaders"]
            .as_object()
            .unwrap()
            .is_empty()); // 用户自建不动
        assert!(!cfg["providers"][2]["extraHeaders"]
            .as_object()
            .unwrap()
            .is_empty());
    }
}
