//! 用户注册表 (~/.codex-app-transfer/config.json) 读写助手.

use codex_app_transfer_registry::{config_file, load_raw_config, save_raw_config, RawConfig};
use serde_json::{json, Value};

pub fn load() -> Result<RawConfig, String> {
    let path = config_file().ok_or_else(|| "无法定位用户配置目录".to_owned())?;
    if !path.exists() {
        return Ok(json!({
            "version": "1.0.4",
            "activeProvider": null,
            "gatewayApiKey": null,
            "providers": [],
            "settings": {
                "theme": "default",
                "language": "zh",
                "proxyPort": 18080,
                "adminPort": 18081,
                "autoStart": false,
                "autoApplyOnStart": true,
                "exposeAllProviderModels": false,
                "restoreCodexOnExit": true,
                "updateUrl": "https://github.com/Cmochance/codex-app-transfer/releases/latest/download/latest.json"
            }
        }));
    }
    load_raw_config(&path).map_err(|e| format!("读取 config.json 失败: {e}"))
}

pub fn save(cfg: &RawConfig) -> Result<(), String> {
    let path = config_file().ok_or_else(|| "无法定位用户配置目录".to_owned())?;
    save_raw_config(&path, cfg).map_err(|e| format!("写入 config.json 失败: {e}"))
}

/// Mask provider 给前端展示:apiKey 字段去除,extraHeaders 清空(可能含敏感
/// 头),其它字段透传 + 加 `hasApiKey` 标记。
pub fn public_provider(p: &Value) -> Value {
    let Some(obj) = p.as_object() else {
        return p.clone();
    };
    let mut out = obj.clone();
    let has_key = out
        .get("apiKey")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    out.remove("apiKey");
    out.remove("extraHeaders");
    out.insert("hasApiKey".into(), Value::Bool(has_key));
    Value::Object(out)
}
