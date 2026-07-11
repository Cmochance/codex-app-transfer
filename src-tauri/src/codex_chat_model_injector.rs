//! Codex Desktop Quick Chat 模型选择器注入器(MOC-323)。
//!
//! **背景**:新版 Codex(26.707+)的 Quick Chat 模型选择器是 **renderer 硬编码**数组
//! (`U=[{id,model,modelLabel,reasoningEffort},…]`,如 `gpt-5.5`/`gpt-5.6-sol`/`auto`),
//! 显示 `GPT-5.5`/`GPT-5.6 Sol`/`Auto` 等固定标签,**不读** coding 侧的
//! `model_catalog_json`。当 transfer 把 Chat 路由到自定义 provider 时,选中某条目会把它的
//! **原始 gpt model id** 发给 `/f/conversation`(见 `chat_conversation.rs`),再经 proxy
//! resolver 按 `openai_model_slot` 映射到 provider 目标模型 —— 与 coding 侧**同一套映射**。
//!
//! **方案**(与 coding 侧「位置一致」):CDP 注入 daemon 每 tick 装幂等 MutationObserver,
//! 对 picker 每个模型条目**按它的 gpt model id 查 coding 映射**(`provider.models` 的
//! `gpt_5_X` 槽 → 目标模型),relabel 成对应目标。因为路由也走同一 resolver,**显示与实际
//! 调用自动一致**,不会出现「显示 A 实际调 B」的假功能。查不到 coding 槽的条目(如 `Auto` /
//! `gpt-5.6-sol`,非 `gpt_5_X` 槽)→ relabel 成**默认目标**(`models.default`,它们本就路由到
//! 默认模型)。
//!
//! **只 relabel 文本,绝不隐藏/删 DOM**:v2 曾用 `display:none` 隐藏「多余项」的行祖先,结果在
//! React app 里连带吞掉整个 chat 面板(祖先 button/容器太大)。v3 起纯文本 relabel,零结构改动。
//!
//! model id 从**显示标签直接反推**(Codex 显示 = `GPT-` + label / title):
//! `text.toLowerCase().replace(/\s+/g,'-')` → `"GPT-5.5"→gpt-5.5`、`"GPT-5.6 Sol"→gpt-5.6-sol`、
//! `"Auto"→auto`,免读 React fiber。reasoning 档位(`Instant`/`Medium`/`High`/`Pro`)不匹配
//! 模型规则 → 不碰。
//!
//! **为何 daemon**:映射随用户在 transfer 切 provider 而变;daemon 每 tick 读活动 provider
//! 重推,自动跟随。CDP 不可达(Codex 没跑)静默跳过本 tick。注入所有 `index.html` target
//! (含 Quick Chat 的 `?initialRoute=/chatgpt/quick-chat`,排除 avatar-overlay)。
//!
//! **开关** `chatCustomModelEnabled`(默认开)。关闭推 remove:断 observer + 从
//! `data-cas-orig` 还原原始标签 + 从 `data-cas-hidden` 还原被隐藏条目。

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use codex_app_transfer_registry::model_alias::MODEL_SLOTS;

use crate::codex_plugin_unlocker::{current_cdp_url, CDP_PORT};
use crate::codex_theme_injector::{drain_until_response, make_msg};

/// 幂等安装脚本模板。占位:`__PAYLOAD_JSON__` = `{ "map": {<gpt-id>:<target>,…}, "default": <target|null> }`
/// (coding 映射,serde_json 转义)、`__VERSION__`(改脚本 bump 强制重装)。
///
/// MutationObserver:任何 DOM 变动 → rAF 去抖后 sweep。对每个叶子模型标签:
/// - 反推 gpt id = `text.toLowerCase().replace(/\s+/g,'-')`
/// - id 命中 `map` → relabel 成 `map[id]`(coding 目标,与路由一致)
/// - 否则若像模型条目(`/^gpt-5/` 或 `auto`)但不在 map → 隐藏其行(coding 侧不显示的多余项)
/// - 其它(reasoning 档位等)→ 不碰
/// relabel 存 `data-cas-orig`、隐藏存 `data-cas-hidden` 供关闭还原。relabel 后新文本不再匹配
/// gpt 规则 → 不重入。文本走 textContent sink(非 innerHTML)+ serde_json 转义 → 无 XSS。
const INSTALL_SCRIPT_TMPL: &str = r##"
(function() {
  var PAYLOAD = __PAYLOAD_JSON__;
  var VERSION = __VERSION__;
  window.__casChatPayload = PAYLOAD;
  function idFromLabel(t) { return t.trim().toLowerCase().replace(/\s+/g, "-"); }
  function looksLikeModel(t) { var s = t.trim(); return /^GPT-5/i.test(s) || /^gpt-5/.test(s) || s.toLowerCase() === "auto"; }
  // **纯 relabel,绝不隐藏/删 DOM 容器**:React app 里隐藏祖先会连带吞掉整块 UI
  // (buggy v2 隐藏 rowOf 祖先 → 整个 chat 面板消失)。非 coding 槽的条目 relabel 成默认目标。
  function targetFor(pl, id) { return pl.map[id] || pl.default || null; }
  function apply(el) {
    if (el.nodeType !== 1) return;
    if (el.children && el.children.length > 1) return;              // 只碰叶子/单子文本元素
    var pl = window.__casChatPayload || { map: {}, default: null };
    var orig = el.getAttribute("data-cas-orig");
    // 已 relabel 过:名字随 provider 变时用原始文本重算刷新
    if (orig !== null) {
      var tgt0 = targetFor(pl, idFromLabel(orig));
      if (tgt0 && el.textContent !== tgt0) el.textContent = tgt0;
      return;
    }
    var t = (el.textContent || "").trim();
    if (!looksLikeModel(t)) return;                                 // reasoning 档位 / 无关文本不碰
    var tgt = targetFor(pl, idFromLabel(t));                        // coding 槽→目标;非槽→默认
    if (tgt && tgt !== t) { el.setAttribute("data-cas-orig", t); el.textContent = tgt; }
  }
  function sweep() {
    try { var all = document.querySelectorAll("span,div,p,button"); for (var i = 0; i < all.length; i++) apply(all[i]); } catch (e) {}
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

/// 卸载脚本:断 observer + 从 `data-cas-orig` 还原标签 + 从 `data-cas-hidden` 还原被隐藏行 +
/// 清全局引用。幂等。
const REMOVE_SCRIPT: &str = r##"
(function() {
  if (window.__casChatObs) { try { window.__casChatObs.disconnect(); } catch (e) {} }
  try {
    var re = document.querySelectorAll("[data-cas-orig]");
    for (var i = 0; i < re.length; i++) {
      var el = re[i]; var o = el.getAttribute("data-cas-orig");
      if (o !== null) el.textContent = o;
      el.removeAttribute("data-cas-orig");
    }
    var hd = document.querySelectorAll("[data-cas-hidden]");
    for (var j = 0; j < hd.length; j++) {
      var h = hd[j]; if (h.style) h.style.display = "";
      h.removeAttribute("data-cas-hidden");
    }
  } catch (e) {}
  delete window.__casChatObs; delete window.__casChatPayload;
  delete window.__casChatModelVersion; delete window.__casChatSweep;
  return "removed";
})()
"##;

/// 脚本版本:改 [`INSTALL_SCRIPT_TMPL`] 逻辑时 bump,令下一 tick 重装覆盖旧 observer。
/// v3:去掉 v2 的隐藏逻辑(隐藏 DOM 祖先会吞整块 chat 面板)→ 纯 relabel。
const SCRIPT_VERSION: u32 = 3;

/// 读 settings 的 `chatCustomModelEnabled`(**默认 true**,与 chat 功能 gate 一致)。
fn chat_model_enabled() -> bool {
    crate::admin::registry_io::load()
        .ok()
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("chatCustomModelEnabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// 活动 provider 的 `models` 对象(`{default, gpt_5_5, gpt_5_4, …}`)。无活动 → 第一个。
fn active_provider_models() -> Option<serde_json::Map<String, Value>> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let active_id = cfg.get("activeProvider").and_then(Value::as_str);
    let providers = cfg.get("providers")?.as_array()?;
    let p = match active_id {
        Some(id) => providers
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(id))?,
        None => providers.first()?,
    };
    p.get("models").and_then(Value::as_object).cloned()
}

/// 构建喂给注入脚本的 payload:`{ "map": { <gpt openai_id>: <目标模型>, … }, "default": <默认目标|null> }`。
///
/// **与 coding 侧 `catalog_models_for_provider` 同源**:遍历 [`MODEL_SLOTS`],对每个有
/// `openai_id` 的槽,取 `provider.models[slot.key]` 非空映射作目标;`gpt_5_5` 槽空则用
/// `default` 填充(对齐 coding catalog 的 MOC-154 行为)。其余空槽跳过 → 注入脚本隐藏对应条目。
/// map 全空且无 default → None(不注入)。
fn chat_relabel_payload() -> Option<String> {
    let models = active_provider_models()?;
    let get = |k: &str| {
        models
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let default_target = get("default");
    let mut map = serde_json::Map::new();
    for slot in MODEL_SLOTS {
        let Some(openai_id) = slot.openai_id else {
            continue;
        };
        let target = get(slot.key).or_else(|| {
            if slot.key == "gpt_5_5" {
                default_target
            } else {
                None
            }
        });
        if let Some(t) = target {
            map.insert(openai_id.to_string(), Value::String(t.to_string()));
        }
    }
    if map.is_empty() && default_target.is_none() {
        return None;
    }
    let payload = json!({
        "map": Value::Object(map),
        "default": default_target.map(|s| Value::String(s.to_string())).unwrap_or(Value::Null),
    });
    serde_json::to_string(&payload).ok()
}

/// 枚举需注入的 page target CDP WS URL:`type=page` + url 含 `index.html`(含 Quick Chat 的
/// `?initialRoute=/chatgpt/quick-chat`)+ 不含 `avatar-overlay`。CDP 未就绪 → Err。
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

/// 在单个 target 上 `Runtime.evaluate` 一段脚本。
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

/// 在所有 chat target 上 eval `script`。任一成功即 Ok(count);无 target / 全失败 Err。
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

/// 常驻 daemon:每 tick 读开关 + 活动 provider 映射,推 install(带 coding 映射 payload)或
/// remove。main.rs 启动 spawn 一次。CDP 不可达(Codex 没跑)静默跳过。
pub async fn run_chat_model_daemon() {
    const TICK: Duration = Duration::from_secs(5);
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
        let Some(payload) = chat_relabel_payload() else {
            continue;
        };
        let script = INSTALL_SCRIPT_TMPL
            .replace("__PAYLOAD_JSON__", &payload)
            .replace("__VERSION__", &SCRIPT_VERSION.to_string());
        match eval_all(&script).await {
            Ok(_) => warned = false,
            Err(e) => {
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
        // **回归护栏**:install 必须纯 relabel,绝不隐藏/删 DOM(v2 隐藏祖先吞掉整块 chat 面板)。
        assert!(!INSTALL_SCRIPT_TMPL.contains("display"));
        assert!(!INSTALL_SCRIPT_TMPL.contains("data-cas-hidden"));
        assert!(!INSTALL_SCRIPT_TMPL.contains("remove()"));
        // remove 必须断 observer + 还原标签 + 还原(旧 v2 可能残留的)隐藏行
        assert!(REMOVE_SCRIPT.contains("disconnect"));
        assert!(REMOVE_SCRIPT.contains("data-cas-orig"));
        assert!(REMOVE_SCRIPT.contains("data-cas-hidden"));
    }

    #[test]
    fn payload_json_escapes_and_substitutes() {
        // 含引号/反斜杠的目标模型名不能破坏脚本。
        let payload = json!({"map":{"gpt-5.5":"gr\"ok\\4.5"},"default":Value::Null}).to_string();
        let script = INSTALL_SCRIPT_TMPL.replace("__PAYLOAD_JSON__", &payload);
        assert!(script.contains(r#""gpt-5.5":"gr\"ok\\4.5""#));
        assert!(!script.contains("__PAYLOAD_JSON__"));
    }

    #[test]
    fn label_to_id_derivation_matches_slots() {
        // 与脚本 idFromLabel 等价:显示标签 → gpt model id,须命中 MODEL_SLOTS 的 openai_id。
        let derive = |t: &str| t.trim().to_lowercase().replace(' ', "-");
        assert_eq!(derive("GPT-5.5"), "gpt-5.5");
        assert_eq!(derive("GPT-5.4"), "gpt-5.4");
        assert_eq!(derive("GPT-5.6 Sol"), "gpt-5.6-sol");
        assert_eq!(derive("Auto"), "auto");
        // gpt-5.5 / gpt-5.4 是真实 slot openai_id;gpt-5.6-sol / auto 不是(→ 会被隐藏)
        assert_eq!(openai_model_slot_id("gpt-5.5"), true);
        assert_eq!(openai_model_slot_id("gpt-5.4"), true);
        assert_eq!(openai_model_slot_id("gpt-5.6-sol"), false);
        assert_eq!(openai_model_slot_id("auto"), false);
    }

    fn openai_model_slot_id(id: &str) -> bool {
        MODEL_SLOTS.iter().any(|s| s.openai_id == Some(id))
    }
}
