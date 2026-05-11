use serde_json::Value;

use crate::types::AdapterError;

use super::session::ResponseSessionCache;

/// 生成 `ResponseSessionPlan.response_id`，供 responses/gemini_native 共用。
pub(crate) fn response_id_for_session() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("resp_{nanos:x}")
}

/// 按 `previous_response_id` 把历史消息与当前消息合并。
///
/// 语义对齐现有 `responses`/`gemini_native` 路径:
/// - cache 命中: 历史 + 当前
/// - cache miss 且当前为空: `PreviousResponseNotFound`
/// - cache miss 且当前非空: 降级为仅当前
/// - 若历史里已有 system/developer 且当前首条是 system,去重当前首 system
pub(crate) fn merge_messages_with_previous_response(
    mut current_messages: Vec<Value>,
    original_body: &Value,
    session_cache: Option<&ResponseSessionCache>,
) -> Result<Vec<Value>, AdapterError> {
    let previous_response_id = original_body
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    if previous_response_id.is_empty() {
        return Ok(current_messages);
    }

    let Some(cache) = session_cache else {
        return Ok(current_messages);
    };

    if let Some(history) = cache.get(previous_response_id) {
        let history_has_system = history.iter().any(|msg| {
            matches!(
                msg.get("role").and_then(|v| v.as_str()),
                Some("system" | "developer")
            )
        });
        if history_has_system
            && current_messages
                .first()
                .and_then(|msg| msg.get("role"))
                .and_then(|v| v.as_str())
                == Some("system")
        {
            current_messages.remove(0);
        }
        let mut messages = history;
        messages.extend(current_messages);
        return Ok(messages);
    }

    if current_messages.is_empty() {
        return Err(AdapterError::PreviousResponseNotFound {
            previous_response_id: previous_response_id.to_owned(),
        });
    }

    Ok(current_messages)
}
