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
//! 调用自动一致**。查不到 coding 槽的条目(如 `Auto` / `gpt-5.6-sol`,非 `gpt_5_X` 槽)→
//! relabel 成**默认目标**(`models.default`,它们本就路由到默认模型)。
//!
//! **只 relabel 文本,绝不隐藏/删 DOM**:v2 曾用 `display:none` 隐藏「多余项」的行祖先,结果在
//! React app 里连带吞掉整个 chat 面板(祖先容器太大)。v3 起纯文本 relabel,零结构改动。
//! 匹配用**锚定的完整模型标签形 + 长度封顶**(v4,code-review C3),避免误伤描述性文案。
//!
//! **注入目标**:Quick Chat 与主窗都是 `app://-/index.html`(quick 带
//! `?initialRoute=/chatgpt/quick-chat`),picker 可能渲染在任一 → 注入**所有** `index.html`
//! target(复用 [`crate::codex_theme_injector::page_target_ws_urls`],排除 avatar-overlay)。
//!
//! **开关**:transfer settings `chatCustomModelEnabled`(**默认开**)。关闭 / 切到无映射
//! provider 时推 remove:断 observer + 从 `data-cas-orig` 还原原始标签(remove 亦兼清旧 v2
//! 可能残留的 `data-cas-hidden`)。

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use codex_app_transfer_registry::model_alias::MODEL_SLOTS;

use crate::codex_theme_injector::{drain_until_response, make_msg, page_target_ws_urls};

/// 幂等安装脚本模板。占位:`__PAYLOAD_JSON__` = `{ "map": {<gpt-id>:<target>,…}, "default": <target|null> }`
/// (coding 映射,serde_json 转义)、`__VERSION__`(改脚本 bump 强制重装)。
///
/// MutationObserver:任何 DOM 变动 → rAF 去抖后 sweep。对每个叶子模型标签:
/// - 反推 gpt id = `text.toLowerCase().replace(/\s+/g,'-')`
/// - id 命中 `map` → relabel 成 `map[id]`(coding 目标,与路由一致)
/// - 否则(非 coding 槽,如 auto / gpt-5.6-sol)→ relabel 成 `default` 目标(它们本就路由到默认)
/// - reasoning 档位 / 描述文案等不匹配 `looksLikeModel` → 不碰
/// relabel 存 `data-cas-orig` 供关闭还原;`data-cas-orig` 早返实现幂等 + 防重入(**非**靠新文本
/// 不匹配 gpt 规则,provider 目标名可能恰好像模型名)。文本走 textContent sink + serde_json 转义 → 无 XSS。
const INSTALL_SCRIPT_TMPL: &str = r##"
(function() {
  var PAYLOAD = __PAYLOAD_JSON__;
  var VERSION = __VERSION__;
  window.__casChatPayload = PAYLOAD;
  function idFromLabel(t) { return t.trim().toLowerCase().replace(/\s+/g, "-"); }
  // 锚定完整模型标签形(GPT-5.5 / GPT-5.6 Sol / GPT-5.4 Codex / Auto)+ 长度封顶,避免误伤
  // 描述性文案(如 "GPT-5.5 is our strongest…")—— code-review C3。
  function looksLikeModel(t) {
    var s = t.trim();
    return s.length <= 24 && (/^gpt-5(\.\d+)?( [a-z0-9][\w .-]{0,20})?$/i.test(s) || s.toLowerCase() === "auto");
  }
  function targetFor(pl, id) { return pl.map[id] || pl.default || null; }
  function apply(el) {
    if (el.nodeType !== 1) return;
    if (el.children && el.children.length > 1) return;              // 只碰叶子/单子文本元素
    var pl = window.__casChatPayload || { map: {}, default: null };
    var orig = el.getAttribute("data-cas-orig");
    if (orig !== null) {                                            // 已 relabel:随 provider 变刷新
      var tgt0 = targetFor(pl, idFromLabel(orig));
      if (tgt0 && el.textContent !== tgt0) el.textContent = tgt0;
      return;
    }
    var t = (el.textContent || "").trim();
    if (!looksLikeModel(t)) return;                                // reasoning 档位 / 无关文案不碰
    var tgt = targetFor(pl, idFromLabel(t));                       // coding 槽→目标;非槽→默认
    if (tgt && tgt !== t) { el.setAttribute("data-cas-orig", t); el.textContent = tgt; }
  }
  function sweep() {
    try { var all = document.querySelectorAll("span,div,p,button"); for (var i = 0; i < all.length; i++) apply(all[i]); }
    catch (e) { window.__casChatSweepErr = String((e && e.message) || e); }   // M5:不静默吞,记 window 供 CDP 排查
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

/// 卸载脚本:断 observer + 从 `data-cas-orig` 还原标签(兼清旧 v2 可能残留的 `data-cas-hidden`
/// 隐藏行)+ 清全局引用。幂等。
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
  delete window.__casChatModelVersion; delete window.__casChatSweep; delete window.__casChatSweepErr;
  return "removed";
})()
"##;

/// 脚本版本:改 [`INSTALL_SCRIPT_TMPL`] 逻辑时 bump,令下一 tick 重装覆盖旧 observer。
/// v3:去掉 v2 隐藏逻辑(隐藏 DOM 祖先会吞整块 chat 面板)→ 纯 relabel。
/// v4:`looksLikeModel` 锚定完整标签形 + 长度封顶,避免误伤描述文案(code-review C3)。
const SCRIPT_VERSION: u32 = 4;

/// 一次性读 auth 守卫补丁落地 breadcrumb(`chat_guard_status.json`,由 `chat_guard_patch.js`
/// 写),`applied=false` 则 warn。守卫没打上 = Chat 请求 auth 被拒、进不了 proxy,是「用户开了
/// 却静默不工作」的头号来源(code-review C2 闭环:让静默失效变可发现)。文件不存在=还没启动过
/// 带补丁的 Codex → 静默。
fn warn_if_guard_failed() {
    let Some(home) = codex_app_transfer_registry::paths::resolve_home() else {
        return;
    };
    let path = home
        .join(".codex-app-transfer")
        .join("chat_guard_status.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    if v.get("applied").and_then(Value::as_bool) == Some(false) {
        let reason = v.get("reason").and_then(Value::as_str).unwrap_or("(未知)");
        tracing::warn!(
            reason,
            "[Chat] auth 守卫补丁未打上 — Chat 自定义模型可能不生效(疑 Codex 更新致守卫形态漂移,需跟进 chat_guard_patch.js)"
        );
    }
}

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

/// 活动 provider 的 relabel payload(读磁盘 → 委托纯函数 [`build_payload_from_models`])。
fn chat_relabel_payload() -> Option<String> {
    build_payload_from_models(&active_provider_models()?)
}

/// 纯函数(便于单测):`provider.models` → 注入脚本 payload JSON
/// `{ "map": { <gpt openai_id>: <目标模型>, … }, "default": <默认目标|null> }`。
///
/// **与 coding 侧 `catalog_models_for_provider` 同源**:遍历 [`MODEL_SLOTS`],对每个有
/// `openai_id` 的槽,取 `models[slot.key]` 非空映射作目标;`gpt_5_5` 槽空则用 `default` 填充
/// (对齐 coding catalog 的 MOC-154 行为)。其余空槽不进 `map` → 注入脚本对这些条目按 `default`
/// 兜底 relabel(注:与 coding catalog **跳过不显示**空槽的行为不同 —— 纯 relabel 不能隐藏条目)。
/// map 全空且无 default → None(不注入)。
fn build_payload_from_models(models: &serde_json::Map<String, Value>) -> Option<String> {
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
    // default_target: Option<&str> → json! 序列化为 null / 字符串(code-simplifier F3)。
    let payload = json!({ "map": Value::Object(map), "default": default_target });
    serde_json::to_string(&payload).ok()
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
/// **部分失败也告警**(code-review H5):否则 Quick Chat 独立窗口注入失败、主窗成功时,那个
/// 窗口的 picker 还是 GPT 名,而 `.is_ok()` 计数会当全成功掩盖掉。
async fn eval_all(script: &str) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let urls = page_target_ws_urls().await?;
    if urls.is_empty() {
        return Err("无 index.html target".into());
    }
    let mut ok = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for u in &urls {
        match eval_in_target(u, script).await {
            Ok(()) => ok += 1,
            Err(e) => failed.push(format!("{u} → {e}")),
        }
    }
    if !failed.is_empty() {
        tracing::warn!(ok, failed = %failed.join("; "), "[ChatModel] 部分 target 注入失败");
    }
    if ok == 0 {
        return Err(format!("所有 target eval 失败: {}", failed.join("; ")).into());
    }
    Ok(ok)
}

/// 常驻 daemon:每 tick 读开关 + 活动 provider 映射,推 install(带 coding 映射 payload)或
/// remove。main.rs 启动 spawn 一次。CDP 不可达(Codex 没跑)是常态,静默;Codex 在跑但 eval
/// 失败则 warn(code-review M4)。切到无映射 provider 时也推 remove(code-review M1)。
pub async fn run_chat_model_daemon() {
    const TICK: Duration = Duration::from_secs(5);
    let mut needs_remove = true;
    let mut last_err: Option<String> = None;
    let mut guard_checked = false;
    loop {
        tokio::time::sleep(TICK).await;
        // 关 → None;开但活动 provider 无任何映射 → None(切到空 provider,要清残留)。
        let payload = if chat_model_enabled() {
            chat_relabel_payload()
        } else {
            None
        };
        let Some(payload) = payload else {
            if needs_remove {
                match eval_all(REMOVE_SCRIPT).await {
                    Ok(_) => needs_remove = false,
                    Err(_) => { /* CDP 不可达是常态,静默重试 */ }
                }
            }
            continue;
        };
        needs_remove = true;
        let script = INSTALL_SCRIPT_TMPL
            .replace("__PAYLOAD_JSON__", &payload)
            .replace("__VERSION__", &SCRIPT_VERSION.to_string());
        match eval_all(&script).await {
            Ok(_) => {
                last_err = None;
                // 首次成功注入 = Codex 在跑 + CDP 通 → 守卫模块已编译、breadcrumb 已写,此刻查一次。
                if !guard_checked {
                    guard_checked = true;
                    warn_if_guard_failed();
                }
            }
            Err(e) => {
                let msg = e.to_string();
                // 区分「Codex 没跑/端口没就绪」(常态,debug)与「Codex 在跑但 eval 失败」(真错,
                // warn);错误变化时重报,不一次性静默(code-review M4)。
                let quiet = msg.contains("端口尚未就绪") || msg.contains("无 index.html target");
                if last_err.as_deref() != Some(msg.as_str()) {
                    if quiet {
                        tracing::debug!(error = %msg, "[ChatModel] 注入跳过(Codex 未就绪)");
                    } else {
                        tracing::warn!(error = %msg, "[ChatModel] 注入失败(Codex 在跑但 eval 失败)");
                    }
                    last_err = Some(msg);
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
    fn build_payload_maps_nonempty_slots_and_skips_empty() {
        let models = json!({
            "default": "grok-4.5", "gpt_5_5": "grok-4.5", "gpt_5_4": "grok-composer-2.5-fast",
            "gpt_5_4_mini": "", "gpt_5_3_codex": "", "gpt_5_2": ""
        })
        .as_object()
        .unwrap()
        .clone();
        let v: Value = serde_json::from_str(&build_payload_from_models(&models).unwrap()).unwrap();
        assert_eq!(v["map"]["gpt-5.5"], "grok-4.5");
        assert_eq!(v["map"]["gpt-5.4"], "grok-composer-2.5-fast");
        assert!(v["map"].get("gpt-5.4-mini").is_none()); // 空槽跳过
        assert!(v["map"].get("gpt-5.2").is_none());
        assert_eq!(v["default"], "grok-4.5");
    }

    #[test]
    fn build_payload_gpt_5_5_empty_filled_with_default() {
        let models = json!({"default": "grok-4.5", "gpt_5_5": "", "gpt_5_4": ""})
            .as_object()
            .unwrap()
            .clone();
        let v: Value = serde_json::from_str(&build_payload_from_models(&models).unwrap()).unwrap();
        assert_eq!(v["map"]["gpt-5.5"], "grok-4.5"); // gpt_5_5 空 → default 填充(对齐 coding)
        assert!(v["map"].get("gpt-5.4").is_none()); // 其它空槽不填
    }

    #[test]
    fn build_payload_all_empty_is_none() {
        // 全空(含纯空白)且无 default → None,daemon 靠它跳过/清残留。
        let models = json!({"default": "", "gpt_5_5": "  ", "gpt_5_4": ""})
            .as_object()
            .unwrap()
            .clone();
        assert!(build_payload_from_models(&models).is_none());
    }

    #[test]
    fn slot_openai_ids_present_for_derivation() {
        // gpt-5.5 / gpt-5.4 是真实 slot openai_id(picker label 反推后命中 → relabel 成映射目标);
        // gpt-5.6-sol / auto 不是 slot(→ 走 default 兜底 relabel,非隐藏)。
        let is_slot = |id: &str| MODEL_SLOTS.iter().any(|s| s.openai_id == Some(id));
        assert!(is_slot("gpt-5.5"));
        assert!(is_slot("gpt-5.4"));
        assert!(!is_slot("gpt-5.6-sol"));
        assert!(!is_slot("auto"));
    }

    #[test]
    fn label_to_id_derivation_collapses_whitespace() {
        // 与脚本 idFromLabel 等价:`\s+`→`-`(折叠空白**串**,非逐个空格 —— test-analyzer A)。
        let derive = |t: &str| {
            t.trim()
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("-")
        };
        assert_eq!(derive("GPT-5.5"), "gpt-5.5");
        assert_eq!(derive("GPT-5.4"), "gpt-5.4");
        assert_eq!(derive("GPT-5.6 Sol"), "gpt-5.6-sol");
        assert_eq!(derive("GPT-5.6  Sol"), "gpt-5.6-sol"); // 双空格也折叠成单 `-`
        assert_eq!(derive("Auto"), "auto");
    }
}
