//! [MOC-234] responses passthrough 的 **orphan function_call 400 降级:重建完整上下文**。
//!
//! ## 问题
//! Codex 工具续轮用 `previous_response_id` + **只发增量 input**(`function_call_output`),
//! 依赖上游按 prev_id 回查上一轮产生的 `function_call`。部分第三方 Responses 反代(如
//! new-api)在 `store:false` 下**不持久化自己的响应** → 续轮找不到 function_call → 400
//! `No tool call found for function call output with call_id ...`,且**整段会话上下文上游也没有**
//! (远端拼接失效)。只补缺失的 function_call 不够 —— 上游知道用什么工具,但不知道干什么。
//!
//! ## 降级:重建完整上下文(error-path only,仅覆盖全程在 responses 路径的对话)
//! proxy 用 always-on 观测镜像([`crate::responses::passthrough_observe`],每轮记 input+output)
//! 沿 `previous_response_id` 链拼出**时序完整历史**,接上本轮 input,**去掉 previous_response_id**
//! (已 inline,上游无需也无法回查),透明重发 → 上游拿到自包含的完整请求。
//!
//! **边界**:① 仅当观测 store 记到了该会话的历史才能拼(proxy 重启前的、或**跨 provider 切换前**
//! 在 chat 路径产生的历史不在本 store)→ 拼不了则退回 `response.failed` 显示错误。② 错误路径
//! 上的请求重写(偏离纯 1:1),仅在上游明确报 orphan-400 时触发;成功路径一律不动。

use bytes::Bytes;
use serde_json::Value;

use crate::responses::global_passthrough_observe_store;

/// forward 层用:上游错误 body 是否为「orphan function_call」400(new-api 类反代在
/// `store:false` 下找不到自己产生的 function_call)。**只认无歧义的该错误**,避免误触发
/// 重写重试。错误形如 `{"error":{"message":"No tool call found for function call output ..."}}`;
/// 非 JSON / 裹在 SSE 里时退化为子串匹配。
pub fn is_orphan_function_call_error(error_body: &[u8]) -> bool {
    const MARKER: &str = "No tool call found for function call output";
    if let Ok(v) = serde_json::from_slice::<Value>(error_body) {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .or_else(|| v.get("message").and_then(Value::as_str))
            .unwrap_or("");
        if msg.contains(MARKER) {
            return true;
        }
    }
    std::str::from_utf8(error_body)
        .map(|s| s.contains(MARKER))
        .unwrap_or(false)
}

/// forward 层用:orphan-400 续轮的完整上下文重建。解析 `body` → 沿 `previous_response_id`
/// 链从观测镜像拼出时序完整历史 → 接本轮 input → **去 previous_response_id** → 返回修复后 bytes。
/// 无 prev_id / 观测 store 没记到该链(拼出空)/ 非 JSON → `None`(调用方不重试,退回显示错误)。
pub fn rebuild_orphan_context_bytes(body: &[u8]) -> Option<Bytes> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let prev_id = v
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_owned();

    let history = global_passthrough_observe_store().assemble_chain(&prev_id);
    if history.is_empty() {
        // 观测 store 没记到该链 → 拼不了完整上下文,只补 function_call 也无意义(上游仍无
        // 会话上下文),不重试。**优雅降级**:原 orphan-400 照常 surface,无损坏。可能原因:
        // proxy 重启 / 首轮 / 跨 provider 边界;或上一轮记录的**异步竞态**——观测记录已移出流式
        // 热路径(防卡顿/闪烁)到独立 task,理论上本轮(N+1)orphan 重写可能早于上一轮(N)的
        // task 落库。但 orphan-400 只在上游**拒绝 N+1**(一整个网络往返 + 上游处理后)才触发,
        // 而 task 喂的是 client 已消费的同批 chunk、微秒级即落库,远早于该 400 回来 → 竞态仅在
        // runtime 饿死整个 N+1 往返时才命中,且命中也仅本次不自愈(下一轮链已补齐)。
        return None;
    }

    let current_input = v
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut full = history;
    full.extend(current_input);

    let obj = v.as_object_mut()?;
    obj.insert("input".to_owned(), Value::Array(full));
    obj.remove("previous_response_id");
    serde_json::to_vec(&v).ok().map(Bytes::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::global_passthrough_observe_store;
    use serde_json::json;

    #[test]
    fn detects_orphan_error_only_on_marker() {
        assert!(is_orphan_function_call_error(
            br#"{"error":{"message":"No tool call found for function call output with call_id call_X."}}"#
        ));
        // 裹在非 JSON / SSE 里也认
        assert!(is_orphan_function_call_error(
            b"data: No tool call found for function call output with call_id call_Y."
        ));
        // 其它 400 不误触发
        assert!(!is_orphan_function_call_error(
            br#"{"error":{"message":"Invalid API key"}}"#
        ));
        assert!(!is_orphan_function_call_error(b"rate limited"));
    }

    #[test]
    fn rebuilds_full_context_from_observe_chain_and_drops_prev_id() {
        // 唯一 id 避免与并发测试在全局 store 串。
        let store = global_passthrough_observe_store();
        // turn1(无 prev):user 问 + 上游产生 function_call call_R1。
        store.record_turn(
            "rebuild_r1",
            None,
            vec![
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"do X"}]}),
                json!({"type":"function_call","name":"shell","arguments":"{}","call_id":"call_R1"}),
            ],
        );
        // 续轮(orphan):只发 call_R1 的 output + previous_response_id=rebuild_r1。
        let body = json!({
            "model":"gpt-5.5","stream":true,"store":false,
            "previous_response_id":"rebuild_r1",
            "input":[{"type":"function_call_output","call_id":"call_R1","output":"done"}]
        });
        let out =
            rebuild_orphan_context_bytes(&serde_json::to_vec(&body).unwrap()).expect("应能重建");
        let rebuilt: Value = serde_json::from_slice(&out).unwrap();
        let input = rebuilt["input"].as_array().unwrap();
        // 完整时序:user 问、function_call call_R1、function_call_output call_R1 = 3
        assert_eq!(input.len(), 3, "应拼出完整历史 + 当前轮:{rebuilt}");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_R1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_R1");
        // previous_response_id 被去掉(已 inline)。
        assert!(rebuilt.get("previous_response_id").is_none());
    }

    #[test]
    fn no_rebuild_when_chain_not_recorded() {
        // prev_id 不在观测 store(proxy 重启 / 跨 provider 边界)→ None,不重试。
        let body = json!({
            "previous_response_id":"never_recorded_xyz",
            "input":[{"type":"function_call_output","call_id":"c","output":"o"}]
        });
        assert!(rebuild_orphan_context_bytes(&serde_json::to_vec(&body).unwrap()).is_none());
    }

    #[test]
    fn no_rebuild_without_prev_id() {
        let body = json!({"input":[{"type":"message","role":"user","content":"hi"}]});
        assert!(rebuild_orphan_context_bytes(&serde_json::to_vec(&body).unwrap()).is_none());
    }
}
