//! Apply / restore 主入口.

use serde::{Deserialize, Serialize};

use crate::auth::{read_auth, write_auth};
use crate::model_catalog::{
    catalog_models_for_provider, upsert_catalog_models, CODEX_MODEL_CATALOG_KEY,
};
use crate::paths::CodexPaths;
use crate::snapshot::{
    drop_snapshot, has_snapshot, read_snapshot_auth, read_snapshot_config, snapshot_codex_state,
    snapshot_toml_value_literal,
};
use crate::toml_sync::{sync_root_value, toml_string_literal};
use crate::CodexError;

/// 我们 apply 时实际触碰的 auth 字段(restore 时只动这些,其它字段保留)。
const MANAGED_AUTH_KEYS: &[&str] = &["auth_mode", "OPENAI_API_KEY"];

/// 我们 apply 时实际触碰的 config.toml 根级别字段(restore 时只动这些)。
const MANAGED_TOML_KEYS: &[&str] = &[
    "openai_base_url",
    "model_context_window",
    CODEX_MODEL_CATALOG_KEY,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyConfig<'a> {
    /// 代理 base URL,例如 `http://127.0.0.1:18080`。
    pub base_url: &'a str,
    /// gateway API key(`cas_...`),会写到 auth.json。空字符串表示移除。
    pub gateway_api_key: &'a str,
    /// 当前 active provider 默认模型是否支持 1M 上下文。
    /// 为 `true` 时 config.toml 会被注入 1M 兼容配置。
    pub supports_1m: bool,
    /// 当前 active provider 的展示名,用于生成 Codex model catalog。
    #[serde(default)]
    pub provider_name: &'a str,
    /// 当前 active provider 的默认真实模型 ID,用于生成 Codex model catalog。
    #[serde(default)]
    pub default_model: &'a str,
    /// 应用版本(写入快照 manifest,便于诊断)。
    pub app_version: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplyResult {
    pub config_toml_path: String,
    pub auth_json_path: String,
    pub snapshot_taken: bool,
    pub model_context_window_set: bool,
    pub model_catalog_json_set: bool,
}

/// 把 active provider 配置写入 `~/.codex/{config.toml,auth.json}`,
/// 首次写入前自动 snapshot。
pub fn apply_provider(paths: &CodexPaths, cfg: &ApplyConfig) -> Result<ApplyResult, CodexError> {
    // 1. snapshot(幂等;已有快照不会覆盖)
    let snapshot_taken_now = !has_snapshot(paths);
    snapshot_codex_state(paths, cfg.app_version)?;

    // 2. config.toml: openai_base_url
    if cfg.base_url.is_empty() {
        sync_root_value(&paths.config_toml, "openai_base_url", None)?;
    } else {
        let literal = toml_string_literal(cfg.base_url);
        sync_root_value(&paths.config_toml, "openai_base_url", Some(&literal))?;
    }

    // 3. config.toml: model_context_window(旧版兼容) + model_catalog_json(Codex 0.128+)
    if cfg.supports_1m {
        sync_root_value(&paths.config_toml, "model_context_window", Some("1000000"))?;
        let catalog_literal = toml_string_literal(&paths.model_catalog_json.display().to_string());
        sync_root_value(
            &paths.config_toml,
            CODEX_MODEL_CATALOG_KEY,
            Some(&catalog_literal),
        )?;
        let models = catalog_models_for_provider(cfg.provider_name, cfg.default_model, true);
        upsert_catalog_models(&paths.model_catalog_json, &models)?;
    } else {
        sync_root_value(&paths.config_toml, "model_context_window", None)?;
        sync_root_value(&paths.config_toml, CODEX_MODEL_CATALOG_KEY, None)?;
    }

    // 4. auth.json: auth_mode + OPENAI_API_KEY
    let mut auth = read_auth(&paths.auth_json)?;
    let obj = auth.as_object_mut().expect("read_auth 保证返回 Object");
    if cfg.gateway_api_key.is_empty() {
        obj.remove("OPENAI_API_KEY");
    } else {
        obj.insert(
            "auth_mode".into(),
            serde_json::Value::String("apikey".into()),
        );
        obj.insert(
            "OPENAI_API_KEY".into(),
            serde_json::Value::String(cfg.gateway_api_key.to_owned()),
        );
    }
    write_auth(&paths.auth_json, &auth)?;

    Ok(ApplyResult {
        config_toml_path: paths.config_toml.display().to_string(),
        auth_json_path: paths.auth_json.display().to_string(),
        snapshot_taken: snapshot_taken_now,
        model_context_window_set: cfg.supports_1m,
        model_catalog_json_set: cfg.supports_1m,
    })
}

/// 基于快照精确还原我们改过的 key,不动用户在我们运行期间手加的内容。
/// 还原成功后清掉快照。
pub fn restore_codex_state(paths: &CodexPaths) -> Result<bool, CodexError> {
    if !has_snapshot(paths) {
        // 没快照时退化为旧版"删除我们的 key"逻辑,与 Python 行为对齐
        for key in MANAGED_TOML_KEYS {
            sync_root_value(&paths.config_toml, key, None)?;
        }
        if paths.auth_json.exists() {
            let mut auth = read_auth(&paths.auth_json)?;
            if let Some(obj) = auth.as_object_mut() {
                for key in MANAGED_AUTH_KEYS {
                    obj.remove(*key);
                }
            }
            write_auth(&paths.auth_json, &auth)?;
        }
        return Ok(false);
    }

    // 1. config.toml:对每个 managed key 用快照里的字面量还原;快照里没有就删
    let snapshot_config = read_snapshot_config(paths).unwrap_or_default();
    for key in MANAGED_TOML_KEYS {
        let literal = snapshot_toml_value_literal(&snapshot_config, key);
        sync_root_value(&paths.config_toml, key, literal.as_deref())?;
    }

    // 2. auth.json:对每个 managed key,快照里有就改回快照值,没有就 remove
    let snapshot_auth = read_snapshot_auth(paths);
    let mut current = read_auth(&paths.auth_json)?;
    if let Some(obj) = current.as_object_mut() {
        for key in MANAGED_AUTH_KEYS {
            match snapshot_auth.get(*key) {
                Some(v) => {
                    obj.insert((*key).to_owned(), v.clone());
                }
                None => {
                    obj.remove(*key);
                }
            }
        }
    }
    write_auth(&paths.auth_json, &current)?;

    drop_snapshot(paths)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, CodexPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = CodexPaths::from_home_dir(tmp.path());
        (tmp, paths)
    }

    fn read_toml(paths: &CodexPaths) -> String {
        std::fs::read_to_string(&paths.config_toml).unwrap()
    }

    fn read_auth_value(paths: &CodexPaths) -> serde_json::Value {
        read_auth(&paths.auth_json).unwrap()
    }

    #[test]
    fn apply_on_empty_writes_both_files_and_takes_snapshot() {
        let (_t, paths) = setup();
        let result = apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                app_version: "v2.0.0-stage2.5",
            },
        )
        .unwrap();
        assert!(result.snapshot_taken);
        assert!(!result.model_context_window_set);
        assert!(!result.model_catalog_json_set);

        let toml = read_toml(&paths);
        assert!(toml.contains("openai_base_url = \"http://127.0.0.1:18080\""));
        assert!(!toml.contains("model_context_window"));

        let auth = read_auth_value(&paths);
        assert_eq!(auth["auth_mode"], "apikey");
        assert_eq!(auth["OPENAI_API_KEY"], "cas_test");
    }

    #[test]
    fn apply_with_supports_1m_writes_model_context_window_and_catalog() {
        let (_t, paths) = setup();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "k",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro[1m]",
                app_version: "v",
            },
        )
        .unwrap();
        let toml = read_toml(&paths);
        assert!(toml.contains("model_context_window = 1000000"));
        assert!(toml.contains("model_catalog_json = "));
        assert!(toml.contains(".codex-app-transfer"));
        assert!(toml.contains("config.json"));
        let catalog: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&paths.model_catalog_json).unwrap()).unwrap();
        assert_eq!(catalog["models"][0]["context_window"], 1_000_000);
        assert_eq!(catalog["models"][0]["effective_context_window_percent"], 95);
        assert!(catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["slug"] == "deepseek-v4-pro"));
    }

    #[test]
    fn apply_preserves_user_other_toml_and_auth_fields() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "# my comment\napi_key = \"k\"\n[profiles]\nfoo = 1\n",
        )
        .unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"tokens\":{\"access\":\"xyz\"},\"OPENAI_API_KEY\":\"old\"}\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://up",
                gateway_api_key: "cas_new",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                app_version: "v",
            },
        )
        .unwrap();
        let toml = read_toml(&paths);
        assert!(toml.contains("# my comment"));
        assert!(toml.contains("api_key = \"k\""));
        assert!(toml.contains("openai_base_url = \"http://up\""));
        assert!(toml.contains("[profiles]"));
        assert!(toml.contains("foo = 1"));
        let auth = read_auth_value(&paths);
        assert_eq!(auth["OPENAI_API_KEY"], "cas_new");
        assert_eq!(auth["tokens"]["access"], "xyz", "用户 tokens 不应被动");
    }

    #[test]
    fn restore_with_snapshot_brings_back_original_values() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        // 用户原本的状态:有 base_url 和 auth.OPENAI_API_KEY
        std::fs::write(
            &paths.config_toml,
            "openai_base_url = \"https://api.openai.com/v1\"\n[profiles]\nfoo = 1\n",
        )
        .unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"OPENAI_API_KEY\":\"sk-original\",\"tokens\":{\"a\":1}}\n",
        )
        .unwrap();
        // apply 我们的代理配置
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://127.0.0.1:18080",
                gateway_api_key: "cas_proxy",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro",
                app_version: "v",
            },
        )
        .unwrap();
        // 还原
        let restored = restore_codex_state(&paths).unwrap();
        assert!(restored, "有快照时 restore 应返回 true");

        let toml = read_toml(&paths);
        assert!(
            toml.contains("openai_base_url = \"https://api.openai.com/v1\""),
            "base_url 应还原为原始 OpenAI 地址"
        );
        assert!(
            !toml.contains("model_context_window"),
            "原状态没有 1M 字段,还原后也不应有"
        );
        assert!(toml.contains("[profiles]"), "用户的 [profiles] 应保留");

        let auth = read_auth_value(&paths);
        assert_eq!(auth["OPENAI_API_KEY"], "sk-original");
        assert_eq!(auth["tokens"]["a"], 1);
        assert!(
            auth.get("auth_mode").is_none(),
            "原状态没有 auth_mode,还原后应不存在"
        );

        assert!(!has_snapshot(&paths), "restore 完成后应清掉快照");
    }

    #[test]
    fn restore_without_snapshot_falls_back_to_remove_managed_keys() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "openai_base_url = \"http://leftover\"\nmodel_context_window = 1000000\nfoo = 1\n",
        )
        .unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"leftover\",\"keep\":1}\n",
        )
        .unwrap();
        let restored = restore_codex_state(&paths).unwrap();
        assert!(!restored, "没有快照时返回 false");
        let toml = read_toml(&paths);
        assert!(!toml.contains("openai_base_url"));
        assert!(!toml.contains("model_context_window"));
        assert!(toml.contains("foo = 1"));
        let auth = read_auth_value(&paths);
        assert!(auth.get("OPENAI_API_KEY").is_none());
        assert!(auth.get("auth_mode").is_none());
        assert_eq!(auth["keep"], 1);
    }

    #[test]
    fn apply_then_apply_again_does_not_overwrite_original_snapshot() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(&paths.config_toml, "openai_base_url = \"original\"\n").unwrap();
        // 第一次 apply
        let r1 = apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://first",
                gateway_api_key: "cas_first",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                app_version: "v",
            },
        )
        .unwrap();
        assert!(r1.snapshot_taken);
        // 第二次 apply
        let r2 = apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://second",
                gateway_api_key: "cas_second",
                supports_1m: true,
                provider_name: "DeepSeek",
                default_model: "deepseek-v4-pro",
                app_version: "v",
            },
        )
        .unwrap();
        assert!(!r2.snapshot_taken, "第二次不应再 snapshot");
        // restore 应回到 ORIGINAL,不是 first
        restore_codex_state(&paths).unwrap();
        let toml = read_toml(&paths);
        assert!(toml.contains("openai_base_url = \"original\""));
    }

    #[test]
    fn apply_with_empty_gateway_api_key_removes_key() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.auth_json,
            "{\"OPENAI_API_KEY\":\"present\",\"keep\":1}\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                app_version: "v",
            },
        )
        .unwrap();
        let auth = read_auth_value(&paths);
        assert!(auth.get("OPENAI_API_KEY").is_none());
        assert_eq!(auth["keep"], 1);
    }

    #[test]
    fn apply_with_empty_base_url_removes_key() {
        let (_t, paths) = setup();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "",
                gateway_api_key: "k",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                app_version: "v",
            },
        )
        .unwrap();
        let toml = std::fs::read_to_string(&paths.config_toml).unwrap_or_default();
        assert!(!toml.contains("openai_base_url"));
    }

    /// 防回归:若用户的 config.toml 里某 key 含 `key_alt = ...` 这种前缀同名行,
    /// apply / restore 都不应误改它(已由 toml_sync 单测覆盖,这里再做端到端校验)。
    #[test]
    fn similar_prefixed_keys_are_not_touched() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        std::fs::write(
            &paths.config_toml,
            "openai_base_url_alt = \"keep\"\nopenai_base_url = \"old\"\n",
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://new",
                gateway_api_key: "k",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                app_version: "v",
            },
        )
        .unwrap();
        let toml = read_toml(&paths);
        assert!(toml.contains("openai_base_url_alt = \"keep\""));
        assert!(toml.contains("openai_base_url = \"http://new\""));
    }

    #[test]
    fn auth_json_unaffected_when_user_has_oauth_tokens() {
        let (_t, paths) = setup();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        let oauth_blob = json!({
            "tokens": {
                "access_token": "ya29.xxx",
                "refresh_token": "1//xxx",
                "expires_at": 9999999999i64
            }
        });
        std::fs::write(
            &paths.auth_json,
            serde_json::to_string_pretty(&oauth_blob).unwrap(),
        )
        .unwrap();
        apply_provider(
            &paths,
            &ApplyConfig {
                base_url: "http://x",
                gateway_api_key: "cas_test",
                supports_1m: false,
                provider_name: "Mock",
                default_model: "mock-model",
                app_version: "v",
            },
        )
        .unwrap();
        let auth = read_auth_value(&paths);
        assert_eq!(auth["tokens"]["access_token"], "ya29.xxx");
        assert_eq!(auth["OPENAI_API_KEY"], "cas_test");
        // restore 应把 OAuth 块完整保留,把 OPENAI_API_KEY 删除(原来没有)
        restore_codex_state(&paths).unwrap();
        let auth_after = read_auth_value(&paths);
        assert_eq!(auth_after["tokens"]["access_token"], "ya29.xxx");
        assert!(auth_after.get("OPENAI_API_KEY").is_none());
        assert!(auth_after.get("auth_mode").is_none());
    }
}
