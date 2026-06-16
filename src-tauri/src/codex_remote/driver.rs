//! CodexDriver —— 通过 CDP `Runtime.evaluate` 驱动 Codex Desktop 的 renderer
//! 发对话、读流式输出(MOC-249 移动端远程控制 M1)。
//!
//! **可行性已 spike 实证**(2026-06-16,Codex 26.609 / Chromium 149):
//! - composer = 底部唯一可见 `div.ProseMirror[contenteditable]`(ProseMirror 编辑器)
//! - 灌字 = `focus()` + `document.execCommand('insertText')`(回退 `beforeinput/input`)
//! - 提交 = 点 `button[class*="size-token-button-compose"]`(回退爬 fiber 调 `handleSubmit()`)
//! - 读输出 = `[data-local-conversation-final-assistant]`(剥用户气泡+时间戳),兜底用
//!   `[data-user-message-bubble]` + 排除 composer 的滚动容器(codex-e2e-test skill 实证)
//! - 完成判定 = `[data-local-conversation-final-assistant]` 就绪(final_ready)优先,
//!   `isSubmitting===false` 兜底 —— isSubmitting 在 streaming/Thinking 阶段会提前读 false
//!
//! **边界**(spike 实证):页面 CSP `connect-src` 只放 chatgpt/openai/mapbox,注入 JS
//! 开不了外部 WS → 对外 bot 长连接必须 Rust 侧(见 [`super::telegram`]);renderer 纯
//! 浏览器沙箱(无 Node)。详见 memory `reference_codex_renderer_drive_feasibility`。
//!
//! 复用 [`crate::codex_theme_injector`] 的 CDP 原语(`locate_main_window_ws` /
//! `make_msg` / `drain_until_response`)与 [`crate::codex_plugin_unlocker::CDP_PORT`]。
//!
//! **抗脆弱**:所有 selector / fiber 路径集中在本文件 [`SELECTORS`] 注释区;Codex 升级
//! 漂移时只改这里。bump [`DRIVER_SCHEMA_VERSION`] 便于排查。

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::codex_theme_injector::{drain_until_response, locate_main_window_ws, make_msg};

/// selector/fiber 契约版本(Codex renderer 结构变化时 bump,纯排查标记)。
pub const DRIVER_SCHEMA_VERSION: u32 = 1;

/// 关键 selector(集中管理,Codex 升级回归只改这里):
/// - composer:`.ProseMirror`
/// - 发送钮:`button[class*="size-token-button-compose"]`
/// - 用户气泡:`[class*="bg-token-foreground/5"][class*="rounded-2"]`
/// - 新建对话:`[aria-label^="Start new chat"]`
const SELECTORS: &str = "see module doc";
const _: &str = SELECTORS;

/// 一轮对话当前快照。
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// composer(`.ProseMirror`)是否在当前视图。
    pub composer_present: bool,
    /// composer fiber 的 `isSubmitting`:true=模型正在跑,false=空闲,None=读不到。
    pub submitting: Option<bool>,
    /// 末个用户气泡之后的 transcript 文本(= 最近一轮 assistant 输出,已轻清洗)。
    pub reply: Option<String>,
    /// `[data-local-conversation-final-assistant]` 在场且有非空文本 —— Codex 在**最终**
    /// assistant 答案渲染好后才打这个标记(Thinking/streaming 阶段不在)。比 `isSubmitting`
    /// 更可靠的「本轮已结束」信号(isSubmitting 在 streaming 阶段会提前读 false);用于
    /// fiber 漂移、读不到 submitting 时的完成判定(codex-e2e-test skill 实证)。
    pub final_ready: bool,
}

/// 一次 CDP `Runtime.evaluate`(连接→发→drain→关),返回 JS 的 `returnByValue` 结果。
/// 每调用新建连接(与 [`crate::codex_quota_injector`] 的 `evaluate_once` 同策略)。
async fn cdp_eval(script: &str) -> Result<Option<Value>, String> {
    let ws_url = locate_main_window_ws().await.map_err(|e| e.to_string())?;
    let (ws_stream, _) = connect_async(&ws_url).await.map_err(|e| e.to_string())?;
    let (mut write, mut read) = ws_stream.split();
    let (msg, _) = make_msg(
        1,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
    );
    write
        .send(WsMessage::Text(msg.into()))
        .await
        .map_err(|e| e.to_string())?;
    let value = drain_until_response(&mut read, 1).await?;
    let _ = write.close().await;
    Ok(value)
}

/// 注入页里复用的小工具(fiber 定位 + isSubmitting 读取),拼进各脚本前缀。
const JS_HELPERS: &str = r#"
function __crFiberOf(el){ if(!el) return null; for(var k in el){ if(k.indexOf('__reactFiber$')===0) return el[k]; } return null; }
function __crComposer(){ return document.querySelector('.ProseMirror'); }
function __crSubmitting(){
  var pm=__crComposer(); if(!pm) return null;
  var el=pm, f=null; while(el && !(f=__crFiberOf(el))) el=el.parentElement;
  var d=0; while(f && d<22){ var p=f.memoizedProps; if(p && typeof p.isSubmitting==='boolean') return p.isSubmitting; f=f.return; d++; }
  return null;
}
"#;

/// 读快照:composer 在否 + isSubmitting + finalReady + 最近一轮 assistant 文本。
/// 提取与完成信号(codex-e2e-test skill 实证):
/// - finalReady = `[data-local-conversation-final-assistant]` 在场且有非空文本(最终答案
///   渲染好才出),作完成信号优于 isSubmitting(后者 streaming 阶段提前读 false)。
/// - reply 优先取 final-assistant(剥用户气泡 + 时间戳叶子);兜底用 `data-user-message-bubble`
///   + **排除 composer** 的滚动容器(避免圈进 Approve/模型选择/Thinking 噪声)。
const JS_SNAPSHOT: &str = r#"
(function(){
  var pm=__crComposer();
  var out={ present: !!pm, submitting: __crSubmitting(), reply: null, finalReady: false };
  var fa=document.querySelectorAll('[data-local-conversation-final-assistant]');
  if(fa.length){
    var clone=fa[fa.length-1].cloneNode(true);
    clone.querySelectorAll('[data-user-message-bubble]').forEach(function(n){ n.remove(); });
    clone.querySelectorAll('*').forEach(function(n){ if(!n.children.length && /^\s*\d{1,2}:\d{2}\s?(AM|PM)\s*$/i.test(n.textContent||'')) n.remove(); });
    var t=(clone.innerText||'').trim();
    if(t){ out.reply=t; out.finalReady=true; }
  }
  var ub=document.querySelectorAll('[data-user-message-bubble]');
  if(ub.length){
    var lastUser=ub[ub.length-1];
    var cont=lastUser, picked=null;
    for(var i=0;i<16 && cont.parentElement;i++){
      var c=cont.parentElement;
      if(c.querySelector('.ProseMirror')) break;
      cont=c;
      var r=c.getBoundingClientRect();
      if(r.width>380 && c.scrollHeight>c.clientHeight+20) picked=c;
    }
    var box=picked||cont;
    var full=box.innerText||''; var key=(lastUser.innerText||'').slice(0,60); var idx=key?full.lastIndexOf(key):-1;
    if(out.reply==null) out.reply = idx>=0 ? full.slice(idx + (lastUser.innerText||'').length) : null;
  }
  return out;
})()
"#;

/// 点「新建对话」按钮(任意一个匹配的;远程控制 M1 单项目,取第一个即可)。
const JS_NEW_CHAT: &str = r#"
(function(){
  var b=document.querySelector('[aria-label^="Start new chat"]')||document.querySelector('[aria-label*="new chat" i]');
  if(b){ b.click(); return {clicked:true}; }
  return {clicked:false};
})()
"#;

/// 提交当前 composer:优先点发送钮,disabled / 缺失则爬 fiber 调 `handleSubmit()`。
const JS_SUBMIT: &str = r#"
(function(){
  var sb=document.querySelector('button[class*="size-token-button-compose"]');
  if(sb && !(sb.disabled===true || sb.getAttribute('aria-disabled')==='true')){ sb.click(); return {via:'button'}; }
  var pm=__crComposer(); if(!pm) return {via:'none', err:'no composer'};
  var el=pm, f=null; while(el && !(f=__crFiberOf(el))) el=el.parentElement;
  var d=0, hs=null;
  while(f && d<24){ var p=f.memoizedProps; if(p && typeof p.handleSubmit==='function'){ hs=p.handleSubmit; break; } f=f.return; d++; }
  if(hs){ try{ hs(); return {via:'handleSubmit'}; }catch(e){ return {via:'handleSubmit', err:String(e)}; } }
  return {via:'none'};
})()
"#;

/// 停止当前轮:优先 fiber `onStop()`。按钮回退**仅在确实正在跑(isSubmitting）时**
/// 才点 —— 该 selector 与发送钮同一个,空闲时它是「发送」,点了会把 composer 草稿当
/// 新一轮提交(bot-review P2)。空闲且无 onStop → 返 `none`(无可停)。
const JS_STOP: &str = r#"
(function(){
  var pm=__crComposer(); var el=pm, f=null;
  while(el && !(f=__crFiberOf(el))) el=el.parentElement;
  var d=0; while(f && d<24){ var p=f.memoizedProps; if(p && typeof p.onStop==='function'){ try{ p.onStop(); return {via:'onStop'}; }catch(e){} } f=f.return; d++; }
  if(__crSubmitting()===true){
    var sb=document.querySelector('button[class*="size-token-button-compose"]');
    if(sb){ sb.click(); return {via:'button'}; }
  }
  return {via:'none'};
})()
"#;

/// 读当前快照。
pub async fn snapshot() -> Result<Snapshot, String> {
    let script = format!("{JS_HELPERS}\n{JS_SNAPSHOT}");
    let v = cdp_eval(&script).await?.unwrap_or(Value::Null);
    Ok(Snapshot {
        composer_present: v.get("present").and_then(Value::as_bool).unwrap_or(false),
        submitting: v.get("submitting").and_then(Value::as_bool),
        reply: v
            .get("reply")
            .and_then(Value::as_str)
            .map(clean_reply)
            .filter(|s| !s.is_empty()),
        final_ready: v
            .get("finalReady")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// 点新建对话。返回是否点中。
pub async fn new_chat() -> Result<bool, String> {
    let v = cdp_eval(JS_NEW_CHAT).await?.unwrap_or(Value::Null);
    Ok(v.get("clicked").and_then(Value::as_bool).unwrap_or(false))
}

/// 把 `text` 灌进 composer。**先全选替换**(`selectNodeContents` 不 collapse →
/// `execCommand('insertText')` 替换掉任何残留草稿),避免「残留草稿 + 远程指令」一起
/// 被提交、执行非预期文本。灌完**校验** composer 文本与目标一致(空白不敏感,容忍
/// ProseMirror 对多行的换行规整),不一致返 Err 让上层中止、不提交。
pub async fn set_input(text: &str) -> Result<String, String> {
    let lit = serde_json::to_string(text).map_err(|e| e.to_string())?;
    let script = format!(
        r#"{JS_HELPERS}
(function(){{
  var pm=__crComposer(); if(!pm) return {{ok:false, err:'no composer'}};
  var TEXT={lit};
  function norm(s){{ return (s||'').replace(/\s+/g,''); }}
  pm.focus();
  // 全选现有内容(含旧草稿)→ insertText 直接替换(不 collapse)
  try{{ var sel=window.getSelection(); sel.removeAllRanges(); var r=document.createRange(); r.selectNodeContents(pm); sel.addRange(r); }}catch(e){{}}
  try{{ document.execCommand('insertText', false, TEXT); }}catch(e){{}}
  // 若不匹配(execCommand 失败 / 未替换干净):显式清空 + beforeinput/input 回退
  if(norm(pm.textContent) !== norm(TEXT)){{
    try{{ var s2=window.getSelection(); s2.removeAllRanges(); var r2=document.createRange(); r2.selectNodeContents(pm); s2.addRange(r2); document.execCommand('delete'); }}catch(e){{}}
    try{{
      pm.dispatchEvent(new InputEvent('beforeinput',{{inputType:'insertText',data:TEXT,bubbles:true,cancelable:true}}));
      pm.dispatchEvent(new InputEvent('input',{{inputType:'insertText',data:TEXT,bubbles:true}}));
    }}catch(e){{}}
  }}
  return {{ok: norm(pm.textContent) === norm(TEXT), text: pm.textContent}};
}})()"#
    );
    let v = cdp_eval(&script).await?.unwrap_or(Value::Null);
    if let Some(err) = v.get("err").and_then(Value::as_str) {
        return Err(format!("set_input: {err}"));
    }
    let text_in = v.get("text").and_then(Value::as_str).unwrap_or("");
    // 校验入框内容(空白不敏感)== 目标,防残留草稿混入被提交
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "composer 内容与目标不一致(可能有残留草稿),已中止以防提交非预期文本;当前: {:?}",
            text_in.chars().take(60).collect::<String>()
        ));
    }
    Ok(text_in.to_string())
}

/// 提交当前 composer。返回提交途径(`button` / `handleSubmit`)。
/// JS 侧带 `err` 字段(`via:'none'` 无入口,或 `via:'handleSubmit'` 调用抛异常)一律
/// 透出为 Err —— 否则 handleSubmit 抛异常仍被当成功,导致流式循环空转到超时。
pub async fn submit() -> Result<String, String> {
    let script = format!("{JS_HELPERS}\n{JS_SUBMIT}");
    let v = cdp_eval(&script).await?.unwrap_or(Value::Null);
    let via = v.get("via").and_then(Value::as_str).unwrap_or("none");
    if let Some(err) = v.get("err").and_then(Value::as_str) {
        return Err(format!("submit 失败({via}): {err}"));
    }
    if via == "none" {
        return Err("submit 失败: 没有可用的提交入口".to_string());
    }
    Ok(via.to_string())
}

/// 停止当前轮。
pub async fn stop() -> Result<String, String> {
    let script = format!("{JS_HELPERS}\n{JS_STOP}");
    let v = cdp_eval(&script).await?.unwrap_or(Value::Null);
    Ok(v.get("via")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_string())
}

/// 轻清洗 transcript 尾段:去掉 Codex 的时间戳行(`3:47 PM`)与 `Worked for Xs` 行,
/// 折叠多余空行。M1 够用,精细化留 M2。
fn clean_reply(raw: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // 形如 "3:47 PM" / "11:02 AM" 的时间戳行
        let is_ts = t.len() <= 9
            && (t.ends_with("AM") || t.ends_with("PM"))
            && t.chars().next().is_some_and(|c| c.is_ascii_digit());
        if is_ts {
            continue;
        }
        // "Worked for 0s" / "Worked for 1m 3s" 等状态行;流式思考/批准占位
        if t.starts_with("Worked for ") || t == "Thinking" || t == "Approve for me" {
            continue;
        }
        lines.push(t);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_reply_strips_timestamps_and_status() {
        let raw = "3:47 PM\nWorked for 0s\n\n远程链路已连通\n3:47 PM";
        assert_eq!(clean_reply(raw), "远程链路已连通");
    }

    #[test]
    fn clean_reply_keeps_multiline_body() {
        let raw = "11:02 AM\n第一行\n第二行\nWorked for 2s";
        assert_eq!(clean_reply(raw), "第一行\n第二行");
    }

    #[test]
    fn clean_reply_empty_stays_empty() {
        assert_eq!(clean_reply("\n\n  \n"), "");
    }
}
