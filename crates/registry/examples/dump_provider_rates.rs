//! 导出各 provider 的 Credit 倍率目录为 JSON,供 CI 的 provider-rate-drift 检测脚本对比上游。
//!
//! 用编译后的真实 `QODER_MODELS` / `WORKBUDDY_MODELS`(而非正则解析 .rs)确保口径准确。
//! 运行:`cargo run -q -p codex-app-transfer-registry --example dump_provider_rates`
//! 输出:`{"qoder":[{key,display_name,credit_rate}], "workbuddy":[...]}`(credit_rate 可为 null)。

use codex_app_transfer_registry::qoder_catalog::QODER_MODELS;
use codex_app_transfer_registry::workbuddy_catalog::WORKBUDDY_MODELS;
use serde_json::{json, Value};

fn main() {
    let qoder: Vec<Value> = QODER_MODELS
        .iter()
        .map(|m| {
            json!({
                "key": m.key,
                "display_name": m.display_name,
                "credit_rate": m.credit_rate,
            })
        })
        .collect();
    let workbuddy: Vec<Value> = WORKBUDDY_MODELS
        .iter()
        .map(|m| {
            json!({
                "key": m.key,
                "display_name": m.display_name,
                "credit_rate": m.credit_rate,
            })
        })
        .collect();
    let out = json!({ "qoder": qoder, "workbuddy": workbuddy });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
