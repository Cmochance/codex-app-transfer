//! Codex 主界面顶部「活跃对话条」注入器。
//!
//! 背景:新版 Codex 把会话统一放左栏,活跃对话不会自动顶到最上方。跨项目并行多会话时,
//! 需要频繁滚动左栏定位目标会话。该注入器在主界面顶部注入一个轻量条带:
//! - 展示活跃会话标题 + 状态(current/active)
//! - 点击条目切换会话
//! - 条目右侧 `×` 尝试关闭会话(优先点行内 close,回退到上下文菜单)
//!
//! 架构沿用 quota / stash:daemon 每 tick 通过 CDP `Runtime.evaluate` 推送「幂等 install
//! + update payload」脚本。无 `Page.addScriptToEvaluateOnNewDocument` 注册,页面 reload /
//! Codex 重启后下一 tick 自动重挂。关闭开关后推 remove 脚本清残留。

use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::codex_theme_injector::{drain_until_response, locate_main_window_ws, make_msg};

const MAX_ACTIVE_ITEMS: usize = 16;

const INSTALL_SCRIPT: &str = r##"
(function() {
  var VERSION = 1;
  if (window.__catConvStripInstalled) {
    if (window.__catConvStripVersion === VERSION) {
      try { window.__catConvStripEnsure && window.__catConvStripEnsure(); } catch (e) {}
      return;
    }
    try { if (window.__catConvStripObserver) window.__catConvStripObserver.disconnect(); } catch (e) {}
    ['cat-conv-strip-style', 'cat-conv-strip-entry'].forEach(function(id) {
      var n = document.getElementById(id);
      if (n) n.remove();
    });
  }
  window.__catConvStripVersion = VERSION;
  window.__catConvStripInstalled = true;
  window.__catConvStripData = window.__catConvStripData || { items: [] };
  window.__catConvStripSig = null;
  window.__catConvHrefTpl = window.__catConvHrefTpl || '';

  var UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

  function fiberOf(el) {
    if (!el) return null;
    for (var k in el) {
      if (k.indexOf('__reactFiber$') === 0) return el[k];
    }
    return null;
  }
  function convIdFromFiber(start) {
    if (!start) return null;
    var f = fiberOf(start);
    var n = 0;
    while (f && n < 40) {
      var bags = [f.memoizedProps, f.memoizedState];
      for (var bi = 0; bi < bags.length; bi++) {
        var bag = bags[bi];
        if (!bag || typeof bag !== 'object') continue;
        if (typeof bag.conversationId === 'string' && UUID_RE.test(bag.conversationId)) return bag.conversationId;
        if (typeof bag.id === 'string' && UUID_RE.test(bag.id)) return bag.id;
        for (var key in bag) {
          if (key === 'conversationId' || /[Cc]onversationId$/.test(key)) {
            var v = bag[key];
            if (typeof v === 'string' && UUID_RE.test(v)) return v;
          }
        }
      }
      f = f.return; n++;
    }
    return null;
  }
  function currentConvId() {
    try {
      if (typeof window.__catActiveConvId === 'function') {
        var v = window.__catActiveConvId();
        if (typeof v === 'string' && UUID_RE.test(v)) return v;
      }
    } catch (e) {}
    var anchors = [
      document.querySelector('[aria-label^="Context usage:"]'),
      document.querySelector('.app-shell-main-content-frame'),
      document.querySelector('.main-surface'),
      document.querySelector('#cat-conv-strip-entry'),
    ];
    for (var i = 0; i < anchors.length; i++) {
      var id = convIdFromFiber(anchors[i]);
      if (id) return id;
    }
    return null;
  }

  function sidebarRoot() {
    return (
      document.querySelector('.app-shell-left-panel') ||
      document.querySelector('aside[aria-label]') ||
      document.querySelector('aside') ||
      document.querySelector('nav')
    );
  }
  function rowCandidates() {
    var root = sidebarRoot();
    if (!root) return [];
    var nodes = root.querySelectorAll('a,button,[role="button"],div');
    var out = [];
    var seen = {};
    for (var i = 0; i < nodes.length; i++) {
      var n = nodes[i];
      var id = convIdFromFiber(n);
      if (!id || seen[id]) continue;
      var txt = ((n.textContent || '').replace(/\s+/g, ' ').trim());
      if (!txt) continue;
      seen[id] = true;
      out.push({ id: id, el: n });
    }
    return out;
  }
  function rowById(id) {
    var rows = rowCandidates();
    for (var i = 0; i < rows.length; i++) {
      if (rows[i].id === id) return rows[i].el;
    }
    return null;
  }
  function clickRowById(id) {
    var row = rowById(id);
    if (!row) return false;
    var tgt = row.closest('a,button,[role="button"]') || row;
    try {
      tgt.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 }));
      return true;
    } catch (e) {
      return false;
    }
  }
  function inferHrefTemplate() {
    if (window.__catConvHrefTpl) return window.__catConvHrefTpl;
    var links = document.querySelectorAll('a[href]');
    for (var i = 0; i < links.length; i++) {
      var href = links[i].getAttribute('href') || '';
      var m = href.match(UUID_RE);
      if (!m) continue;
      window.__catConvHrefTpl = href.replace(m[0], '__CAT_CONV_ID__');
      return window.__catConvHrefTpl;
    }
    return '';
  }
  function navigateByTemplate(id) {
    var tpl = inferHrefTemplate();
    if (!tpl) return false;
    var href = tpl.replace('__CAT_CONV_ID__', id);
    try {
      window.location.assign(new URL(href, window.location.href).toString());
      return true;
    } catch (e) {
      return false;
    }
  }
  function openConversation(id) {
    if (!id) return false;
    if (clickRowById(id)) return true;
    return navigateByTemplate(id);
  }

  function findCloseInScope(scope) {
    if (!scope || !scope.querySelector) return null;
    var sels = [
      'button[aria-label*="Close"]',
      'button[aria-label*="close"]',
      'button[aria-label*="关闭"]',
      'button[title*="Close"]',
      'button[title*="close"]',
      'button[title*="关闭"]',
      'button[data-testid*="close"]',
      '[data-testid*="close"] button',
    ];
    for (var i = 0; i < sels.length; i++) {
      var b = scope.querySelector(sels[i]);
      if (b) return b;
    }
    return null;
  }
  function closeMenuItem() {
    var items = document.querySelectorAll('[role="menuitem"],button,[data-radix-collection-item]');
    for (var i = 0; i < items.length; i++) {
      var t = (items[i].textContent || '').trim();
      if (!t) continue;
      if (/^(close|archive)/i.test(t) || /关闭|归档/.test(t)) return items[i];
    }
    return null;
  }
  function closeById(id) {
    var row = rowById(id);
    if (!row) return false;
    try { row.dispatchEvent(new MouseEvent('mouseenter', { bubbles: true })); } catch (e) {}
    var btn = findCloseInScope(row);
    if (btn) {
      try {
        btn.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 }));
        return true;
      } catch (e) {}
    }
    try { row.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, button: 2 })); } catch (e) {}
    var mi = closeMenuItem();
    if (mi) {
      try {
        mi.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 }));
        return true;
      } catch (e) {}
    }
    return false;
  }
  function closeConversation(id) {
    if (!id) return false;
    if (closeById(id)) return true;
    var cur = currentConvId();
    if (cur !== id && openConversation(id)) {
      setTimeout(function() { closeById(id); }, 140);
      return true;
    }
    return false;
  }

  function ensureStyle() {
    if (document.getElementById('cat-conv-strip-style')) return;
    var st = document.createElement('style');
    st.id = 'cat-conv-strip-style';
    st.textContent =
      '#cat-conv-strip-entry{position:sticky;top:0;z-index:35;padding:8px 12px 6px;background:var(--color-token-dropdown-background,rgba(20,24,36,.82));border-bottom:1px solid var(--color-token-border,rgba(128,128,128,.28));backdrop-filter:blur(8px) saturate(120%)}' +
      '#cat-conv-strip-entry .ccs-head{display:flex;align-items:center;gap:8px;margin-bottom:6px;color:var(--color-token-text-tertiary,rgba(238,241,247,.56));font-size:12px}' +
      '#cat-conv-strip-entry .ccs-count{font-variant-numeric:tabular-nums;opacity:.85}' +
      '#cat-conv-strip-entry .ccs-list{display:flex;gap:6px;overflow-x:auto;padding-bottom:2px}' +
      '#cat-conv-strip-entry .ccs-chip{display:inline-flex;align-items:center;gap:8px;min-width:180px;max-width:320px;padding:6px 8px;border-radius:8px;border:1px solid var(--color-token-border,rgba(128,128,128,.28));background:var(--color-token-bg-secondary,rgba(255,255,255,.05));cursor:pointer}' +
      '#cat-conv-strip-entry .ccs-chip:hover{border-color:color-mix(in srgb,var(--cl-accent,#6c83c4) 45%,transparent)}' +
      '#cat-conv-strip-entry .ccs-chip.is-current{border-color:color-mix(in srgb,var(--cl-accent,#6c83c4) 65%,transparent);background:color-mix(in srgb,var(--cl-accent,#6c83c4) 14%,transparent)}' +
      '#cat-conv-strip-entry .ccs-main{display:flex;flex-direction:column;gap:2px;min-width:0;flex:1 1 auto}' +
      '#cat-conv-strip-entry .ccs-title{font-size:12.5px;color:var(--color-token-text-primary,#ededed);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}' +
      '#cat-conv-strip-entry .ccs-status{font-size:11px;color:var(--color-token-text-secondary,#8c8782);text-transform:lowercase}' +
      '#cat-conv-strip-entry .ccs-close{display:inline-flex;align-items:center;justify-content:center;width:18px;height:18px;border-radius:999px;color:var(--color-token-text-secondary,#8c8782);font-size:12px;line-height:1;flex:0 0 auto}' +
      '#cat-conv-strip-entry .ccs-close:hover{background:rgba(128,128,128,.2);color:var(--cl-accent,#6c83c4)}';
    (document.head || document.documentElement).appendChild(st);
  }
  function el(tag, cls, txt) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (txt != null) e.textContent = txt;
    return e;
  }
  function hostNode() {
    return (
      document.querySelector('.app-shell-main-content-frame') ||
      document.querySelector('.main-surface') ||
      document.querySelector('[container-name="home-main-content"]') ||
      null
    );
  }
  function render(node, items, currentId) {
    node.textContent = '';
    var head = el('div', 'ccs-head');
    head.appendChild(el('span', null, 'Active conversations'));
    head.appendChild(el('span', 'ccs-count', String(items.length)));
    node.appendChild(head);
    var list = el('div', 'ccs-list');
    for (var i = 0; i < items.length; i++) {
      var it = items[i];
      var isCurrent = !!(currentId && it.id === currentId);
      var chip = el('div', 'ccs-chip' + (isCurrent ? ' is-current' : ''));
      chip.setAttribute('role', 'button');
      chip.setAttribute('tabindex', '0');
      chip.title = it.title || it.id;
      chip.addEventListener('click', (function(id) {
        return function() { openConversation(id); };
      })(it.id));
      chip.addEventListener('keydown', (function(id) {
        return function(ev) {
          if (ev.key === 'Enter' || ev.key === ' ') {
            ev.preventDefault();
            openConversation(id);
          }
        };
      })(it.id));
      var main = el('div', 'ccs-main');
      main.appendChild(el('span', 'ccs-title', it.title || it.id));
      main.appendChild(el('span', 'ccs-status', isCurrent ? 'current' : (it.status || 'active')));
      chip.appendChild(main);
      var close = el('span', 'ccs-close', '×');
      close.title = 'Close';
      close.addEventListener('click', (function(id) {
        return function(ev) {
          ev.stopPropagation();
          closeConversation(id);
        };
      })(it.id));
      chip.appendChild(close);
      list.appendChild(chip);
    }
    node.appendChild(list);
  }
  function ensureNode() {
    var data = window.__catConvStripData;
    var items = (data && Array.isArray(data.items)) ? data.items : [];
    var host = hostNode();
    var node = document.getElementById('cat-conv-strip-entry');
    if (!host || !items.length) {
      if (node) node.remove();
      window.__catConvStripSig = null;
      return;
    }
    ensureStyle();
    var fresh = !node;
    if (!node) {
      node = document.createElement('div');
      node.id = 'cat-conv-strip-entry';
    }
    if (node.parentElement !== host || host.firstElementChild !== node) {
      host.insertBefore(node, host.firstElementChild);
    }
    var currentId = currentConvId();
    var sig = items.map(function(it) {
      return (it.id || '') + '#' + (it.title || '') + '#' + (it.status || '');
    }).join('|') + '|cur=' + (currentId || '');
    if (fresh || sig !== window.__catConvStripSig) {
      render(node, items, currentId);
      window.__catConvStripSig = sig;
    }
  }

  window.__catConvStripEnsure = ensureNode;
  window.__catConvStripUpdate = function(data) {
    window.__catConvStripData = (data && Array.isArray(data.items)) ? data : { items: [] };
    window.__catConvStripSig = null;
    ensureNode();
  };

  var scheduled = false;
  var mo = new MutationObserver(function() {
    if (scheduled) return;
    scheduled = true;
    requestAnimationFrame(function() {
      scheduled = false;
      ensureNode();
    });
  });
  mo.observe(document.body, { childList: true, subtree: true });
  window.__catConvStripObserver = mo;
  ensureNode();
})();
"##;

const REMOVE_SCRIPT: &str = r#"
(function() {
  try { if (window.__catConvStripObserver) window.__catConvStripObserver.disconnect(); } catch (e) {}
  ['cat-conv-strip-style', 'cat-conv-strip-entry'].forEach(function(id) {
    var n = document.getElementById(id);
    if (n) n.remove();
  });
  try {
    delete window.__catConvStripInstalled;
    delete window.__catConvStripVersion;
    delete window.__catConvStripObserver;
    delete window.__catConvStripEnsure;
    delete window.__catConvStripUpdate;
    delete window.__catConvStripData;
    delete window.__catConvStripSig;
    delete window.__catConvHrefTpl;
  } catch (e) {}
})();
"#;

fn strip_enabled() -> bool {
    crate::admin::registry_io::load()
        .ok()
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("codexActiveConversationsEnabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn fallback_title(cwd: &std::path::Path, id: &str) -> String {
    let cwd_base = cwd.file_name().and_then(|s| s.to_str()).unwrap_or_default();
    let short_id: String = id.chars().take(8).collect();
    if cwd_base.is_empty() {
        format!("Session {short_id}")
    } else {
        format!("{cwd_base} ({short_id})")
    }
}

fn collect_active_items() -> Vec<serde_json::Value> {
    let codex_home = match codex_app_transfer_codex_integration::CodexPaths::from_home_env() {
        Ok(p) => p.codex_home,
        Err(_) => return Vec::new(),
    };
    let sessions = match codex_app_transfer_conversation_export::list_sessions(&codex_home) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    sessions
        .into_iter()
        .filter(|s| {
            matches!(
                s.kind,
                codex_app_transfer_conversation_export::RolloutKind::Active
            )
        })
        .take(MAX_ACTIVE_ITEMS)
        .map(|s| {
            let title = s
                .title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| fallback_title(&s.cwd, &s.id));
            json!({
                "id": s.id,
                "title": title,
                "status": "active",
            })
        })
        .collect()
}

enum StripPushError {
    Connect(String),
    Evaluate(String),
}

async fn evaluate_once(script: &str) -> Result<(), StripPushError> {
    let ws_url = locate_main_window_ws()
        .await
        .map_err(|e| StripPushError::Connect(e.to_string()))?;
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .map_err(|e| StripPushError::Connect(e.to_string()))?;
    let (mut write, mut read) = ws_stream.split();
    let (msg, _) = make_msg(
        1,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
    );
    write
        .send(WsMessage::Text(msg.into()))
        .await
        .map_err(|e| StripPushError::Evaluate(e.to_string()))?;
    drain_until_response(&mut read, 1)
        .await
        .map_err(StripPushError::Evaluate)?;
    let _ = write.close().await;
    Ok(())
}

async fn push_strip(items: Vec<serde_json::Value>) -> Result<(), StripPushError> {
    let payload = json!({ "items": items });
    let script = format!(
        "{INSTALL_SCRIPT}\nwindow.__catConvStripUpdate && window.__catConvStripUpdate({payload});"
    );
    evaluate_once(&script).await
}

pub async fn run_active_conversation_strip_daemon() {
    const TICK: Duration = Duration::from_secs(5);
    let mut needs_remove = true;
    let mut evaluate_warned = false;
    loop {
        tokio::time::sleep(TICK).await;
        if strip_enabled() {
            needs_remove = true;
            let items = collect_active_items();
            match push_strip(items).await {
                Ok(()) => evaluate_warned = false,
                Err(StripPushError::Connect(e)) => {
                    tracing::debug!(error = %e, "[ConvStrip] CDP 未就绪,跳过本次注入");
                }
                Err(StripPushError::Evaluate(e)) => {
                    if !evaluate_warned {
                        tracing::warn!(error = %e, "[ConvStrip] 注入 evaluate 失败(选择子可能随 Codex 升级失效)");
                        evaluate_warned = true;
                    }
                }
            }
        } else if needs_remove {
            match evaluate_once(REMOVE_SCRIPT).await {
                Ok(()) => needs_remove = false,
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_has_idempotent_guard_and_update_hook() {
        assert!(INSTALL_SCRIPT.contains("window.__catConvStripInstalled"));
        assert!(INSTALL_SCRIPT.contains("__catConvStripVersion === VERSION"));
        assert!(INSTALL_SCRIPT.contains("window.__catConvStripUpdate"));
    }

    #[test]
    fn remove_script_cleans_nodes_and_globals() {
        assert!(REMOVE_SCRIPT.contains("cat-conv-strip-style"));
        assert!(REMOVE_SCRIPT.contains("cat-conv-strip-entry"));
        assert!(REMOVE_SCRIPT.contains("delete window.__catConvStripInstalled"));
    }

    #[test]
    fn fallback_title_uses_cwd_basename_when_available() {
        let t = fallback_title(
            std::path::Path::new("/tmp/my-project"),
            "019ec12f-abcd-1234-9988-abcdefabcdef",
        );
        assert_eq!(t, "my-project (019ec12f)");
        let t2 = fallback_title(
            std::path::Path::new("/"),
            "019ec12f-abcd-1234-9988-abcdefabcdef",
        );
        assert_eq!(t2, "Session 019ec12f");
    }
}
