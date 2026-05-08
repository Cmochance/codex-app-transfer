//! 用户注册表 (~/.codex-app-transfer/config.json) 读写助手.

use codex_app_transfer_registry::{
    config_file, heal_builtin_provider_fields, load_raw_config, save_raw_config, RawConfig,
};
use serde_json::{json, Value};

pub fn load() -> Result<RawConfig, String> {
    let path = config_file().ok_or_else(|| "cannot locate user config directory".to_owned())?;
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
    let mut cfg = load_raw_config(&path).map_err(|e| format!("read config.json failed: {e}"))?;
    // 强制覆盖 builtin provider 的"非用户配置"字段(apiFormat / authScheme /
    // extraHeaders) — 详见 codex_app_transfer_registry::healing 模块说明。
    // 老版本(v1.x)写入或用户手改可能让这些字段不对(空字符串 / "responses" /
    // 缺失等),触发 MiMo 404 / Kimi 403 等绕过代理的功能性 bug。
    //
    // 策略:有改动 → **写回磁盘**(2026-05-08 用户确认:这类内部协议路由信号
    // 不支持用户自定义,直接覆盖磁盘旧配置,以后不再因残留而出错)。
    if heal_builtin_provider_fields(&mut cfg) {
        // 写回失败不致命:内存里 heal 过的版本仍可用,下次启动再尝试同步盘
        if let Err(e) = save_raw_config(&path, &cfg) {
            eprintln!("warning: write back config.json after heal failed (in-memory healed version still in effect for this session): {e}");
        }
    }
    Ok(cfg)
}

pub fn save(cfg: &RawConfig) -> Result<(), String> {
    let path = config_file().ok_or_else(|| "cannot locate user config directory".to_owned())?;
    save_raw_config(&path, cfg).map_err(|e| format!("write config.json failed: {e}"))
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
