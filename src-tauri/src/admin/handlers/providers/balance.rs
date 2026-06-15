//! Provider 余额 / 用量查询.

use std::time::Duration;

use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use serde_json::{json, Value};

use super::super::super::registry_io::load as load_registry;
use super::super::common::err;
use super::test::{provider_test_error_label, provider_test_headers};
use super::{clean_base_url, provider_api_key};

fn provider_kind(provider: &Value) -> &'static str {
    let probe = format!(
        "{} {}",
        provider.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    )
    .to_ascii_lowercase();
    if probe.contains("deepseek") {
        "deepseek"
    } else if probe.contains("siliconflow") {
        "siliconflow"
    } else if probe.contains("openrouter") {
        "openrouter"
    } else if probe.contains("novita") {
        "novita"
    } else if probe.contains("stepfun") || probe.contains("step") {
        "stepfun"
    } else if probe.contains("moonshot") {
        // Kimi (月之暗面) PAYG:baseUrl 含 `api.moonshot.cn`/`.ai`。**只认 `moonshot` 不认 `kimi`**:
        // 订阅制 `kimi-code`(`api.kimi.com/coding`)不含 `moonshot`、是另一个 provider,不在此列。
        "moonshot"
    } else {
        "unknown"
    }
}

fn balance_endpoint(provider: &Value) -> Option<(&'static str, String)> {
    let kind = provider_kind(provider);
    let base = clean_base_url(
        provider
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    )
    .to_ascii_lowercase();
    match kind {
        "deepseek" => Some((kind, "https://api.deepseek.com/user/balance".to_owned())),
        "siliconflow" => {
            let host = if base.contains(".com") {
                "https://api.siliconflow.com"
            } else {
                "https://api.siliconflow.cn"
            };
            Some((kind, format!("{host}/v1/user/info")))
        }
        "openrouter" => Some((kind, "https://openrouter.ai/api/v1/credits".to_owned())),
        "novita" => Some((kind, "https://api.novita.ai/v3/user/balance".to_owned())),
        "stepfun" => Some((kind, "https://api.stepfun.com/v1/accounts".to_owned())),
        "moonshot" => {
            let host = if base.contains("moonshot.ai") {
                "https://api.moonshot.ai"
            } else {
                "https://api.moonshot.cn"
            };
            Some((kind, format!("{host}/v1/users/me/balance")))
        }
        _ => None,
    }
}

fn float_or_none(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) if !s.is_empty() => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn money_item(
    label: impl Into<String>,
    remaining: Option<f64>,
    total: Option<f64>,
    used: Option<f64>,
    unit: impl Into<String>,
) -> Value {
    json!({
        "label": label.into(),
        "remaining": remaining,
        "total": total,
        "used": used,
        "unit": unit.into(),
    })
}

fn normalize_balance_payload(kind: &str, payload: &Value) -> Vec<Value> {
    if kind == "deepseek" {
        let mut items = Vec::new();
        for item in payload
            .get("balance_infos")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let currency = obj
                .get("currency")
                .and_then(|v| v.as_str())
                .unwrap_or("CNY")
                .to_owned();
            items.push(money_item(
                currency.clone(),
                float_or_none(obj.get("total_balance")),
                float_or_none(obj.get("granted_balance")),
                float_or_none(obj.get("topped_up_balance")),
                currency,
            ));
        }
        return items;
    }

    if kind == "openrouter" {
        let data = payload.get("data").unwrap_or(payload);
        let total = float_or_none(data.get("total_credits"));
        let used = float_or_none(data.get("total_usage"));
        let remaining = match (total, used) {
            (Some(total), Some(used)) => Some(total - used),
            _ => None,
        };
        return vec![money_item("credits", remaining, total, used, "USD")];
    }

    if kind == "moonshot" {
        // 响应:{"code":0,"data":{"available_balance":..,"voucher_balance":..,"cash_balance":..}}.
        // available = 可用余额(= cash + voucher)。响应无币种字段 → 这里默认按 .cn 记 CNY(国际站
        // .ai 罕用);Codex UI 实时显示那条会按 host 推币种符号(见 moonshot_quota)。
        let data = payload.get("data").unwrap_or(payload);
        let Some(obj) = data.as_object() else {
            return Vec::new();
        };
        let mut items = Vec::new();
        if let Some(avail) = float_or_none(obj.get("available_balance")) {
            items.push(money_item("balance", Some(avail), None, None, "CNY"));
        }
        if let Some(cash) = float_or_none(obj.get("cash_balance")) {
            items.push(money_item("cash", Some(cash), None, None, "CNY"));
        }
        if let Some(voucher) = float_or_none(obj.get("voucher_balance")) {
            items.push(money_item("voucher", Some(voucher), None, None, "CNY"));
        }
        return items;
    }

    let data = payload.get("data").unwrap_or(payload);
    if let Some(obj) = data.as_object() {
        for remaining_key in [
            "balance",
            "remaining",
            "available_balance",
            "availableBalance",
            "credit",
        ] {
            if obj.contains_key(remaining_key) {
                let unit = obj
                    .get("currency")
                    .or_else(|| obj.get("unit"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                return vec![money_item(
                    "balance",
                    float_or_none(obj.get(remaining_key)),
                    float_or_none(
                        obj.get("total")
                            .or_else(|| obj.get("totalBalance"))
                            .or_else(|| obj.get("total_credits")),
                    ),
                    float_or_none(
                        obj.get("used")
                            .or_else(|| obj.get("usage"))
                            .or_else(|| obj.get("usedBalance")),
                    ),
                    unit,
                )];
            }
        }
    }
    Vec::new()
}

async fn query_provider_usage_impl(provider: &Value) -> Value {
    if provider_api_key(provider).is_empty() {
        return json!({"success": false, "message": "save the API key first"});
    }
    let Some((kind, endpoint)) = balance_endpoint(provider) else {
        return json!({
            "success": true,
            "supported": false,
            "items": [],
            "message": "balance / usage endpoint is not implemented for this provider yet",
        });
    };

    let headers = provider_test_headers(provider, false);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return json!({
                "success": true,
                "supported": true,
                "ok": false,
                "message": format!("query failed: {}", provider_test_error_label(&error)),
                "items": [],
            });
        }
    };
    let response = match client.get(&endpoint).headers(headers).send().await {
        Ok(response) => response,
        Err(error) => {
            return json!({
                "success": true,
                "supported": true,
                "ok": false,
                "message": format!("query failed: {}", provider_test_error_label(&error)),
                "items": [],
            });
        }
    };
    if !response.status().is_success() {
        return json!({
            "success": true,
            "supported": true,
            "ok": false,
            "statusCode": response.status().as_u16(),
            "message": format!("balance endpoint returned HTTP {}", response.status().as_u16()),
            "items": [],
        });
    }
    let payload = match response.json::<Value>().await {
        Ok(payload) => payload,
        Err(_) => {
            return json!({
                "success": true,
                "supported": true,
                "ok": false,
                "message": "balance endpoint returned a non-JSON response",
                "items": [],
            });
        }
    };
    let items = normalize_balance_payload(kind, &payload);
    let ok = !items.is_empty();
    let message = if ok {
        "query complete"
    } else {
        "balance endpoint response did not contain a recognizable balance field"
    };
    json!({
        "success": true,
        "supported": true,
        "ok": ok,
        "endpoint": endpoint,
        "items": items,
        "message": message,
    })
}

pub async fn query_provider_usage(Path(id): Path<String>) -> impl IntoResponse {
    let cfg = match load_registry() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let provider = cfg
        .get("providers")
        .and_then(|v| v.as_array())
        .and_then(|providers| {
            providers.iter().find(|provider| {
                provider
                    .as_object()
                    .and_then(|o| o.get("id"))
                    .and_then(|v| v.as_str())
                    == Some(id.as_str())
            })
        });
    let Some(provider) = provider else {
        return err(StatusCode::NOT_FOUND, "provider not found").into_response();
    };
    let result = query_provider_usage_impl(provider).await;
    Json(result).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_usage_preserves_legacy_no_key_and_unsupported_payloads() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let no_key = json!({
                "name": "DeepSeek",
                "baseUrl": "https://api.deepseek.com",
            });
            let result = query_provider_usage_impl(&no_key).await;
            assert_eq!(result["success"], json!(false));
            assert_eq!(result["message"], json!("save the API key first"));

            let unsupported = json!({
                "name": "Unknown",
                "baseUrl": "https://api.example.com/v1",
                "apiKey": "test-key",
            });
            let result = query_provider_usage_impl(&unsupported).await;
            assert_eq!(result["success"], json!(true));
            assert_eq!(result["supported"], json!(false));
            assert_eq!(result["items"], json!([]));
        });
    }

    #[test]
    fn balance_payloads_match_legacy_normalization() {
        let deepseek = normalize_balance_payload(
            "deepseek",
            &json!({
                "balance_infos": [{
                    "currency": "CNY",
                    "total_balance": "8.5",
                    "granted_balance": "10",
                    "topped_up_balance": "1.5"
                }]
            }),
        );
        assert_eq!(deepseek[0]["label"], json!("CNY"));
        assert_eq!(deepseek[0]["remaining"], json!(8.5));
        assert_eq!(deepseek[0]["total"], json!(10.0));
        assert_eq!(deepseek[0]["used"], json!(1.5));

        let openrouter = normalize_balance_payload(
            "openrouter",
            &json!({"data": {"total_credits": 12.0, "total_usage": 5.25}}),
        );
        assert_eq!(openrouter[0]["label"], json!("credits"));
        assert_eq!(openrouter[0]["remaining"], json!(6.75));
        assert_eq!(openrouter[0]["unit"], json!("USD"));

        let generic = normalize_balance_payload(
            "siliconflow",
            &json!({"data": {"availableBalance": "3.25", "totalBalance": "4", "usedBalance": "0.75", "currency": "CNY"}}),
        );
        assert_eq!(generic[0]["remaining"], json!(3.25));
        assert_eq!(generic[0]["total"], json!(4.0));
        assert_eq!(generic[0]["used"], json!(0.75));
        assert_eq!(generic[0]["unit"], json!("CNY"));
    }

    /// 关键隔离:`kimi (月之暗面)` PAYG(moonshot host)→ balance 支持;订阅制 `kimi-code`
    /// (`api.kimi.com/coding`)→ 不被识别成 moonshot、无 balance 端点。两者绝不混淆。
    #[test]
    fn moonshot_kind_separates_kimi_paygo_from_kimi_code() {
        let kimi_paygo =
            json!({"name": "Kimi (月之暗面)", "baseUrl": "https://api.moonshot.cn/v1"});
        assert_eq!(provider_kind(&kimi_paygo), "moonshot");
        let (kind, endpoint) = balance_endpoint(&kimi_paygo).expect("moonshot endpoint");
        assert_eq!(kind, "moonshot");
        assert_eq!(endpoint, "https://api.moonshot.cn/v1/users/me/balance");

        // 订阅制 kimi-code:host 是 api.kimi.com,**不含 moonshot** → unknown、无 balance 端点。
        let kimi_code = json!({"name": "Kimi Code", "baseUrl": "https://api.kimi.com/coding/v1"});
        assert_eq!(provider_kind(&kimi_code), "unknown");
        assert!(balance_endpoint(&kimi_code).is_none());

        // 国际站 .ai → 切到 api.moonshot.ai 端点。
        let kimi_intl = json!({"name": "Moonshot", "baseUrl": "https://api.moonshot.ai/v1"});
        let (_, endpoint) = balance_endpoint(&kimi_intl).expect("moonshot.ai endpoint");
        assert_eq!(endpoint, "https://api.moonshot.ai/v1/users/me/balance");
    }

    #[test]
    fn moonshot_balance_normalizes_available_and_breakdown() {
        let items = normalize_balance_payload(
            "moonshot",
            &json!({
                "code": 0,
                "data": {"available_balance": 49.58, "voucher_balance": 46.58, "cash_balance": 3.0},
                "status": true
            }),
        );
        assert_eq!(items[0]["label"], json!("balance"));
        assert_eq!(items[0]["remaining"], json!(49.58));
        assert_eq!(items[0]["unit"], json!("CNY"));
        assert_eq!(items[1]["label"], json!("cash"));
        assert_eq!(items[1]["remaining"], json!(3.0));
        assert_eq!(items[2]["label"], json!("voucher"));
        assert_eq!(items[2]["remaining"], json!(46.58));

        // 缺 available_balance → 空(ok=false,前端显「暂未识别到余额」)。
        assert!(normalize_balance_payload("moonshot", &json!({"data": {}})).is_empty());
    }
}
