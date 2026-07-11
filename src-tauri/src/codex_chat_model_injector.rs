//! Codex Desktop Quick Chat 模型名注入器(MOC-323)。
//!
//! **背景**:新版 Codex(26.707+)的 Quick Chat 模型选择器是 **renderer 硬编码**的
//! 数组(`[{id,model,modelLabel,reasoningEffort},…]`),显示 `GPT-5.6 Sol` / `GPT-5.5`
//! 等固定标签,**不向上游拉模型列表**。当 transfer 把 Chat 路由到自定义 provider
//! (见 `crates/proxy/src/chat_conversation.rs`)时,实际调用的是第三方模型(如
//! `grok-4.5`),但 picker 仍显示 GPT 名 —— 用户无法看出在跟哪个模型对话。
//!
//! **方案**:跟 [`crate::codex_quota_injector`] 同构的 CDP 注入 daemon —— 每 tick 经
//! `Runtime.evaluate` 装一个**幂等 + 版本化**的 MutationObserver,把 picker 里所有
//! `GPT-5.x` 模型标签**按文本匹配** relabel 成活动 provider 的**映射目标模型**
//! (`provider.models.default`,回落 `provider.name`)。**不**碰 reasoning 档位
//! (`Instant`/`Medium`/`High`/`Pro` —— 那是 effort,仍有效)。
//!
//! **为何 daemon 而非 `addScriptToEvaluateOnNewDocument`**:显示名随用户在 transfer
//! 里切 provider 而变;daemon 每 tick 读活动 provider 重推,自动跟随切换(theme 那种
//! 一次注册无法更新名字)。CDP 不可达(Codex 没跑)时静默跳过本 tick,是常态非错误。
//!
//! **注入目标**:Quick Chat 与主窗都是 `app://-/index.html`(quick 带
//! `?initialRoute=/chatgpt/quick`),picker 可能渲染在任一 → 注入**所有** `index.html`
//! target(排除 `avatar-overlay` 宠物悬浮窗),不赌单个主窗。
//!
//! **开关**:transfer settings `chatCustomModelEnabled`(**默认开**,与
//! `admin::services::desktop::process::chat_launch_env` gate 一致)。关闭时推一次
//! remove 脚本:断开 observer + 从 `data-cas-orig` 还原原始 GPT 标签。

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::codex_plugin_unlocker::{current_cdp_url, CDP_PORT};
use crate::codex_theme_injector::{drain_until_response, make_msg};

/// 幂等安装脚本模板。占位:`__NAME_JSON__`(serde_json 转义后的显示名 JS 字面量)、
/// `__VERSION__`(改脚本时 bump 强制重装)。
///
/// 装一个 MutationObserver:任何 DOM 变动 → requestAnimationFrame 去抖后 sweep 全页,
/// 把叶子元素中**文本形如 `GPT-5.x`** 的标签 `textContent` 换成 `window.__casChatModel`。
/// relabel 前把原文存进 `data-cas-orig`(供关闭时还原)。设置新文本触发的 mutation 因新
/// 文本不再匹配 `GPT-5.x` 而不重入,无限循环。已装同版本 → 只刷新名字 + 补 sweep 一次。
///
/// 文本 sink 走 `textContent`(非 innerHTML),名字经 serde_json 转义 → 无 XSS。
const INSTALL_SCRIPT_TMPL: &str = r##"
(function() {
  var NAME = __NAME_JSON__;
  var VERSION = __VERSION__;
  window.__casChatModel = NAME;
  // GPT-5.x 模型标签:GPT-5、GPT-5.6 Sol、GPT-5.5、GPT-5.4 Codex 等;不匹配 Instant/Medium 档位。
  var RE = /^GPT-5(\.\d+)?([  ][A-Za-z0-9 .\-]{1,24})?$/;
  function relabel(el) {
    if (el.nodeType !== 1) return;
    if (el.children && el.children.length > 1) return;         // 只碰叶子/单子文本元素,不砸带图标的容器
    var t = (el.textContent || "").trim();
    var orig = el.getAttribute("data-cas-orig");
    if (orig !== null) {                                        // 已 relabel 过:名字变了就刷新
      var name0 = window.__casChatModel;
      if (name0 && el.textContent !== name0) el.textContent = name0;
      return;
    }
    if (RE.test(t)) {
      var name = window.__casChatModel;
      if (name) { el.setAttribute("data-cas-orig", t); el.textContent = name; }
    }
  }
  function sweep() {
    try { var all = document.querySelectorAll("span,div,p,button"); for (var i = 0; i < all.length; i++) relabel(all[i]); } catch (e) {}
  }
  window.__casChatSweep = sweep;
  if (window.__casChatModelVersion === VERSION) { sweep(); return "refreshed"; }
  window.__casChatModelVersion = VERSION;
  if (window.__casChatObs) { try { window.__casChatObs.disconnect(); } catch (e) {} }
  var pending = false;
  function schedule() {
    if (pending) return; pending = true;
    (window.requestAnimationFrame || window.setTimeout)(function () { pending = false; sweep(); }, 0);
  }
  var obs = new MutationObserver(schedule);
  obs.observe(document.documentElement, { childList: true, subtree: true, characterData: true });
  window.__casChatObs = obs;
  sweep();
  return "installed";
})()
"##;

/// 卸载脚本:断开 observer + 从 `data-cas-orig` 还原原始 GPT 标签 + 清全局引用。幂等。
const REMOVE_SCRIPT: &str = r##"
(function() {
  if (window.__casChatObs) { try { window.__casChatObs.disconnect(); } catch (e) {} }
  try {
    var els = document.querySelectorAll("[data-cas-orig]");
    for (var i = 0; i < els.length; i++) {
      var el = els[i]; var o = el.getAttribute("data-cas-orig");
      if (o !== null) el.textContent = o;
      el.removeAttribute("data-cas-orig");
    }
  } catch (e) {}
  delete window.__casChatObs; delete window.__casChatModel;
  delete window.__casChatModelVersion; delete window.__casChatSweep;
  return "removed";
})()
"##;

/// 脚本版本:改 [`INSTALL_SCRIPT_TMPL`] 逻辑时 bump,令下一 tick 重装覆盖旧 observer。
const SCRIPT_VERSION: u32 = 1;

/// 读 settings 的 `chatCustomModelEnabled`(**默认 true**,与 chat 功能 gate 一致)。
/// 关闭时 Chat 走真实 ChatGPT,picker 显示 GPT 名本就正确 → 不 relabel。
fn chat_model_enabled() -> bool {
    crate::admin::registry_io::load()
        .ok()
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("chatCustomModelEnabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// 活动 provider 要在 picker 显示的名字:优先**映射目标模型**(`models.default`,即
/// Chat/coding 实际调用的模型,与 coding 侧映射同源),回落 provider `name`。无活动
/// provider → providers 第一个;全空 → None(不 relabel)。
fn active_chat_model_display() -> Option<String> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let active_id = cfg.get("activeProvider").and_then(Value::as_str);
    let providers = cfg.get("providers")?.as_array()?;
    let p = match active_id {
        Some(id) => providers
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(id))?,
        None => providers.first()?,
    };
    let target = p
        .get("models")
        .and_then(|m| m.get("default"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(t) = target {
        return Some(t.to_string());
    }
    p.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 枚举所有需要注入的 Codex page target 的 CDP WS URL:`type=page` + url 含
/// `index.html`(含 Quick Chat 的 `?initialRoute=/chatgpt/quick`)+ 不含
/// `avatar-overlay`(宠物悬浮窗)。CDP 未就绪 → Err(caller 静默跳过)。
async fn chat_target_ws_urls() -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    if CDP_PORT.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return Err("CDP 端口尚未就绪".into());
    }
    let url = current_cdp_url();
    let resp = reqwest::get(&url).await?;
    if !resp.status().is_success() {
        return Err(format!("CDP /json/list returned {}", resp.status()).into());
    }
    let pages: Vec<Value> = resp.json().await?;
    let wss = pages
        .iter()
        .filter(|p| {
            p.get("type").and_then(Value::as_str) == Some("page")
                && p.get("url")
                    .and_then(Value::as_str)
                    .map(|u| u.contains("index.html") && !u.contains("avatar-overlay"))
                    .unwrap_or(false)
        })
        .filter_map(|p| {
            p.get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    Ok(wss)
}

/// 在单个 target 上 `Runtime.evaluate` 一段脚本(不关心返回值)。
async fn eval_in_target(
    ws_url: &str,
    script: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(ws_url).await?;
    let (mut write, mut read) = ws_stream.split();
    let (msg, _) = make_msg(
        1,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
    );
    write.send(WsMessage::Text(msg.into())).await?;
    drain_until_response(&mut read, 1).await?;
    let _ = write.close().await;
    Ok(())
}

/// 在所有 chat target 上 eval `script`。任一 target 成功即算这 tick 成功(返 Ok(count));
/// 无 target / 全失败返 Err。
async fn eval_all(script: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let urls = chat_target_ws_urls().await?;
    if urls.is_empty() {
        return Err("无 index.html target".into());
    }
    let mut ok = 0usize;
    for u in &urls {
        if eval_in_target(u, script).await.is_ok() {
            ok += 1;
        }
    }
    if ok == 0 {
        return Err("所有 target eval 失败".into());
    }
    Ok(ok)
}

/// 常驻 daemon:每 tick 读 `chatCustomModelEnabled` + 活动 provider,推 install(带
/// 当前显示名)或 remove。在 main.rs 启动时 spawn 一次。CDP 不可达(Codex 没跑 /
/// 端口未就绪)是常态,静默跳过本 tick,不刷日志。
pub async fn run_chat_model_daemon() {
    const TICK: Duration = Duration::from_secs(5);
    // 初始 true:transfer 重启后开关可能已关而上会话的 relabel 还挂在页面上,首个 off
    // tick 推一次 remove 清残留;失败保持 true 下 tick 重试,成功才复位。开→关同样置 true。
    let mut needs_remove = true;
    let mut warned = false;
    loop {
        tokio::time::sleep(TICK).await;
        if !chat_model_enabled() {
            if needs_remove {
                match eval_all(REMOVE_SCRIPT).await {
                    Ok(_) => needs_remove = false,
                    Err(_) => { /* CDP 不可达是常态,静默重试 */ }
                }
            }
            continue;
        }
        needs_remove = true;
        let Some(name) = active_chat_model_display() else {
            continue;
        };
        let name_json = serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".to_string());
        let script = INSTALL_SCRIPT_TMPL
            .replace("__NAME_JSON__", &name_json)
            .replace("__VERSION__", &SCRIPT_VERSION.to_string());
        match eval_all(&script).await {
            Ok(_) => warned = false,
            Err(e) => {
                // 首次失败 debug 一行(Codex 没跑时每 tick 都失败,不刷屏)。
                if !warned {
                    warned = true;
                    tracing::debug!(error = %e, "[ChatModel] 注入跳过(Codex 未就绪?)");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_is_idempotent_and_versioned() {
        assert!(INSTALL_SCRIPT_TMPL.contains("__casChatModelVersion === VERSION"));
        assert!(INSTALL_SCRIPT_TMPL.contains("data-cas-orig"));
        assert!(INSTALL_SCRIPT_TMPL.contains("MutationObserver"));
        // remove 必须断 observer + 还原原文
        assert!(REMOVE_SCRIPT.contains("disconnect"));
        assert!(REMOVE_SCRIPT.contains("data-cas-orig"));
    }

    #[test]
    fn name_json_escapes_into_js_literal() {
        // 含引号/反斜杠的 provider 名不能破坏脚本(XSS/语法)。
        let name = "Gr\"ok\\4.5";
        let j = serde_json::to_string(name).unwrap();
        let script = INSTALL_SCRIPT_TMPL.replace("__NAME_JSON__", &j);
        assert!(script.contains(r#""Gr\"ok\\4.5""#));
        assert!(!script.contains("__NAME_JSON__"));
    }

    #[test]
    fn model_label_regex_matches_gpt_not_reasoning() {
        // 编译期锚 RE 语义(与脚本内 RE 手动对齐):relabel 只碰 GPT-5.x,放过 Instant/Medium。
        let re = regex_lite_ok();
        for good in [
            "GPT-5",
            "GPT-5.6 Sol",
            "GPT-5.5",
            "GPT-5.4 Codex",
            "GPT-5.3",
        ] {
            assert!(re(good), "should match model label: {good}");
        }
        for bad in ["Instant", "Medium", "High", "Pro", "GPT-4", "Grok"] {
            assert!(!re(bad), "should NOT match: {bad}");
        }
    }

    // 手写等价匹配器(脚本 RE 是 JS 字面量,Rust 侧用等价逻辑做语义回归)。
    fn regex_lite_ok() -> impl Fn(&str) -> bool {
        |t: &str| {
            let t = t.trim();
            let Some(rest) = t.strip_prefix("GPT-5") else {
                return false;
            };
            if rest.is_empty() {
                return true; // "GPT-5"
            }
            // 允许 .<digits> 后跟可选 " <label>"
            let rest = rest.strip_prefix('.').map_or(rest, |r| {
                let digits: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
                &r[digits.len()..]
            });
            rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\u{a0}')
        }
    }
}
