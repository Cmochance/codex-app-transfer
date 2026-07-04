//! remoteChatAsk body 构造:客户端 **OpenAI chat completions** 请求 → QoderWork 私有
//! envelope。schema 逆向自 SDK `PfA`/`buildRemoteChatAskForLoop`(见 MOC-297)。
//!
//! 产出的 JSON 交给 [`crate::QoderAuth::prepare_signed_request`] 作 body,由 WASM
//! 签名 + 加密后 `POST gateway.qoder.com.cn/algo/api/v2/service/pro/sse/agent_chat_generation`。

use serde_json::{json, Value};

/// [`build_remote_chat_ask`] 的输入。id 类字段由调用方生成(uuid)。
pub struct RemoteChatAskParams<'a> {
    /// 客户端原始 OpenAI chat completions 请求体。
    pub openai_body: &'a Value,
    /// 解析后的 QoderWork model key(如 `q36fmodel`),同时作 `X-Model-Key`。
    pub model_key: &'a str,
    /// model source(通常 `"system"`)。
    pub model_source: &'a str,
    /// 会话 id(uuid),与 `Cosy` 头的 session 对应。
    pub session_id: &'a str,
    /// 本次请求 id(uuid)。
    pub request_id: &'a str,
    /// 请求集 id(uuid;单请求可 = `request_id`)。
    pub request_set_id: &'a str,
}

/// 构造 remoteChatAsk envelope。定值字段(chat_task/version/agent_id …)对齐 SDK。
pub fn build_remote_chat_ask(p: &RemoteChatAskParams) -> Value {
    let (system, messages) = split_system(p.openai_body.get("messages"));
    let tools = p
        .openai_body
        .get("tools")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let parameters = build_parameters(p.openai_body);
    json!({
        "request_id": p.request_id,
        "request_set_id": p.request_set_id,
        "chat_record_id": p.request_id,
        "session_id": p.session_id,
        "stream": true,
        "chat_task": "FREE_INPUT",
        "is_reply": true,
        "is_retry": false,
        "source": 1,
        "version": "3",
        "agent_id": "agent_common",
        "task_id": "common",
        "aliyun_user_type": "",
        "model_config": { "key": p.model_key, "source": p.model_source },
        "system": system,
        "messages": messages,
        "tools": tools,
        "parameters": parameters,
    })
}

/// 抽出 system 提示到顶层 `system` 串,并把 system 置于 messages 首位(对齐 SDK
/// `g.unshift({role:"system",content:A})`)。非字符串 content 的 system 消息保留在
/// messages 里不丢。
fn split_system(messages: Option<&Value>) -> (String, Value) {
    let Some(Value::Array(arr)) = messages else {
        return (String::new(), json!([]));
    };
    let mut system_parts = Vec::new();
    let mut rest = Vec::with_capacity(arr.len());
    for m in arr {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match (role, m.get("content").and_then(|c| c.as_str())) {
            ("system", Some(text)) => system_parts.push(text.to_string()),
            _ => rest.push(m.clone()),
        }
    }
    let system = system_parts.join("\n");
    let mut out = Vec::with_capacity(rest.len() + 1);
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }
    out.extend(rest);
    (system, Value::Array(out))
}

/// generation 参数(客户端给才带):`max_tokens` / `reasoning_effort` / `context_length`。
fn build_parameters(ob: &Value) -> Value {
    let mut p = serde_json::Map::new();
    for key in ["max_tokens", "reasoning_effort", "context_length"] {
        if let Some(v) = ob.get(key) {
            p.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_remote_chat_ask_shape() {
        let openai = json!({
            "model": "gpt-4",
            "messages": [
                {"role":"system","content":"you are helpful"},
                {"role":"user","content":"hi"}
            ],
            "max_tokens": 1024,
            "tools": [{"type":"function","function":{"name":"f"}}]
        });
        let p = RemoteChatAskParams {
            openai_body: &openai,
            model_key: "q36fmodel",
            model_source: "system",
            session_id: "s",
            request_id: "r",
            request_set_id: "rs",
        };
        let b = build_remote_chat_ask(&p);
        assert_eq!(b["chat_task"], "FREE_INPUT");
        assert_eq!(b["version"], "3");
        assert_eq!(b["agent_id"], "agent_common");
        assert_eq!(b["stream"], true);
        assert_eq!(b["model_config"]["key"], "q36fmodel");
        assert_eq!(b["model_config"]["source"], "system");
        assert_eq!(b["system"], "you are helpful");
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][1]["role"], "user");
        assert_eq!(b["parameters"]["max_tokens"], 1024);
        assert_eq!(b["tools"][0]["type"], "function");
    }

    #[test]
    fn no_system_message_yields_empty_system() {
        let openai = json!({ "messages": [{"role":"user","content":"hi"}] });
        let p = RemoteChatAskParams {
            openai_body: &openai,
            model_key: "q36fmodel",
            model_source: "system",
            session_id: "s",
            request_id: "r",
            request_set_id: "rs",
        };
        let b = build_remote_chat_ask(&p);
        assert_eq!(b["system"], "");
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["tools"], json!([]));
    }
}
