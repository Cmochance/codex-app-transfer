//! Codex 草稿暂存(Stash)注入器 —— 经 CDP 向 Codex 渲染进程注入「输入框草稿暂存区」。
//!
//! 解决的痛点:用户常在输入框预输入下一轮内容,但上一轮助手回复需要先补充追问时,
//! 只能删掉预输入内容去回复,回头再重打。Stash 提供一个「先存起来、随后选择性恢复/发送」
//! 的草稿暂存区,比 Codex 原生 steer(预存后只能按队列顺序自动发送)更灵活。
//!
//! 三个交互面(全部 renderer 本地,纯 DOM + localStorage,无 Rust↔JS 数据流):
//! - **暂存按钮**(composer 工具栏):存当前输入框内容 → 清空输入框;输入框空时禁用。
//! - **快捷恢复按钮**(composer 工具栏):0 条隐藏 / 1 条点击直接恢复 / >1 条点击唤起下拉选。
//! - **Stash 面板**(pinned-summary 弹窗里、Usage 面板下面):列全部暂存项,每行 恢复/发送/删除。
//!
//! 架构与 [`crate::codex_quota_injector`] 同构:daemon 每 tick 经 CDP `Runtime.evaluate` 跑
//! 幂等 `INSTALL_SCRIPT`(版本化 guard,升级即覆盖旧注入,免重启 Codex)。所有状态在 renderer
//! 侧 localStorage(`catStash`),跨 Codex reload 存活(与 quota 的 usageCache 同机制)。
//!
//! 存储 schema:`localStorage['catStash'] = [{ id, text, ts }]`(全局,不按会话隔离)。
//!
//! 恢复语义(swap,不丢草稿):输入框空 → 直接填入目标项;输入框非空 → 先把当前内容暂存,
//! 再填入目标项。恢复/发送均「消费」该条(git stash pop 模型)。
//!
//! 面板挂载点与 quota 面板同一 scroller。为保证「Stash 在 Usage 下面」的稳定顺序
//! `[...React sections, quota, stash]`,[`crate::codex_quota_injector`] 的占位逻辑做了配合:
//! 当 `#cat-stash-entry` 在场时,quota 面板把自己 `insertBefore` 到 stash 之前(否则两者
//! 每 tick 争抢 lastElementChild)。

use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::codex_theme_injector::{drain_until_response, locate_main_window_ws, make_msg};

const INSTALL_SCRIPT: &str = r##"
(function() {
  // 版本化幂等 guard(对齐 quota injector):同版本仅补一次 ensure 后返回;版本变(应用升级
  // 后本脚本改了)→ 先拆旧 observer + 注入节点再重装,新逻辑免重启 Codex 即覆盖旧注入。
  var VERSION = 1;
  if (window.__catStashInstalled) {
    if (window.__catStashVersion === VERSION) { try { window.__catStashEnsure && window.__catStashEnsure(); } catch (e) {} return; }
    try { if (window.__catStashObserver) window.__catStashObserver.disconnect(); } catch (e) {}
    ['cat-stash-entry', 'cat-stash-bar', 'cat-stash-menu', 'cat-stash-style'].forEach(function(id){ var n = document.getElementById(id); if (n) n.remove(); });
  }
  window.__catStashVersion = VERSION;
  window.__catStashSig = null; // 面板 body 重建签名(见 ensurePanel),重装时清空强制首刷

  // ── 存储(全局 localStorage,跨 reload 存活)──
  var KEY = 'catStash';
  // load 时逐项规整:id/text 强制 string、ts 兜底,屏蔽 schema 漂移或外部写入的非字符串值
  // (否则 renderPanel 的 it.text.replace 会 throw 进而整个面板/按钮静默消失)。
  function load() {
    try {
      var s = localStorage.getItem(KEY); var a = s ? JSON.parse(s) : [];
      if (!Array.isArray(a)) return [];
      return a.filter(function (x) { return x && x.id != null; }).map(function (x) {
        return { id: String(x.id), text: x.text == null ? '' : String(x.text), ts: x.ts || 0 };
      });
    } catch (e) { return []; }
  }
  function save(a) { try { localStorage.setItem(KEY, JSON.stringify(a)); } catch (e) {} }
  // id 用「ms + 会话内单调序号 + 随机」:序号保证同毫秒/同 random 也不碰撞(碰撞会让
  // filter(x=>x.id!==id) 误删两条)。
  var __seq = 0;
  function uid() { return Date.now().toString(36) + '-' + (++__seq).toString(36) + Math.random().toString(36).slice(2, 6); }
  function add(text) { var a = load(); a.push({ id: uid(), text: text, ts: Date.now() }); save(a); }
  function findItem(arr, id) { for (var i = 0; i < arr.length; i++) { if (arr[i].id === id) return arr[i]; } return null; }

  // ── composer 操作(.ProseMirror,选择子均 CDP 实证)──
  function composerEl() { return document.querySelector('.ProseMirror'); }
  function composerText() { var e = composerEl(); return e ? (e.textContent || '') : ''; }
  function fiberOf(el) { if (!el) return null; for (var k in el) { if (k.indexOf('__reactFiber$') === 0) return el[k]; } return null; }
  // 设输入框内容:focus → 全选删除 → insertText(空则只清);execCommand 不生效时回退派发 InputEvent。
  // **返回是否真落值**(caller 据此决定是否消费 stash,绝不在没落值时丢草稿):目标空 → 须真空;
  // 目标非空 → textContent 须等于目标。
  function setComposer(text) {
    var e = composerEl(); if (!e) return false;
    try { e.focus(); } catch (x) {}
    try { document.execCommand('selectAll', false, null); } catch (x) {}
    try { document.execCommand('delete', false, null); } catch (x) {}
    if (text) {
      try { document.execCommand('insertText', false, text); } catch (x) {}
      if ((composerText() || '') !== text) {
        try {
          e.dispatchEvent(new InputEvent('beforeinput', { inputType: 'insertText', data: text, bubbles: true, cancelable: true }));
          e.dispatchEvent(new InputEvent('input', { inputType: 'insertText', data: text, bubbles: true }));
        } catch (x) {}
      }
    }
    var now = composerText() || '';
    return text ? (now === text) : (now.trim() === '');
  }
  // 提交:优先点 compose 按钮,但**运行态(按钮为 Stop)不点**——点它会停掉当前轮次;
  // 改走 fiber handleSubmit()(空闲时发送,运行时入队为 steer,符合「运行中也能发送」语义)。
  // disabled 同样跳过点按钮、回退 fiber。
  function submitComposer() {
    var btn = document.querySelector('button[class*="size-token-button-compose"]');
    var running = !!(btn && /stop/i.test(btn.getAttribute('aria-label') || ''));
    if (btn && !btn.disabled && !running) { try { btn.click(); return true; } catch (e) {} }
    var pm = composerEl(); if (!pm) return false;
    var el = pm, f = null; while (el && !(f = fiberOf(el))) el = el.parentElement;
    var d = 0; while (f && d < 24) { var p = f.memoizedProps; if (p && typeof p.handleSubmit === 'function') { try { p.handleSubmit(); return true; } catch (e) { return false; } } f = f.return; d++; }
    return false;
  }

  // ── 核心动作 ──
  // 恢复:**先**把目标填进输入框,落值成功**才**消费该条(失败则 stash 原封不动,绝不丢草稿)。
  // 输入框非空时把当前内容先暂存(swap)。
  function restore(id) {
    var arr = load(); var item = findItem(arr, id); if (!item) return;
    var cur = composerText();
    if (!setComposer(item.text)) return;   // composer 不可用/未落值 → 不消费
    var rest = arr.filter(function (x) { return x.id !== id; });
    if (cur && cur.trim()) rest.push({ id: uid(), text: cur, ts: Date.now() });
    save(rest); closeMenu(); ensure();
  }
  // 发送:先填入(失败即止,stash 不动);被挤出的当前内容 cur 无论成败都先存好(不丢);
  // 仅在提交成功时才消费目标项,提交失败则保留目标项(此时 item.text 在输入框可重发)。
  function sendItem(id) {
    var arr = load(); var item = findItem(arr, id); if (!item) return;
    var cur = composerText();
    if (!setComposer(item.text)) return;   // composer 不可用/未落值 → 全不动
    var curEntry = cur && cur.trim() ? [{ id: uid(), text: cur, ts: Date.now() }] : [];
    if (submitComposer()) {
      save(arr.filter(function (x) { return x.id !== id; }).concat(curEntry)); // 成功:消费 item + 保留 cur
    } else {
      save(arr.concat(curEntry));          // 失败:保留 item(输入框里可重发)+ 保留 cur
    }
    ensure();
  }
  function del(id) { save(load().filter(function (x) { return x.id !== id; })); ensure(); }
  // 暂存当前输入框内容 → 清空输入框。空内容 no-op。**先清空成功才入库**,避免清空失败时
  // 草稿既进了 stash 又留在输入框(重复)。
  function stashCurrent() { var t = composerText(); if (!t || !t.trim()) return; if (!setComposer('')) return; add(t); ensure(); }

  // ── 样式(scoped,跟随 Codex 主题 token + 注入主题 accent)──
  function ensureStyle() {
    if (document.getElementById('cat-stash-style')) return;
    var st = document.createElement('style'); st.id = 'cat-stash-style';
    st.textContent =
      // 面板(对齐 quota 面板视觉)
      '#cat-stash-entry{display:block;padding:0 0 6px;user-select:none}' +
      '#cat-stash-entry .cshdr{display:flex;align-items:center;height:28px;padding:0 10px 2px 16px;background:var(--color-token-dropdown-background,rgba(20,24,36,.78))}' +
      '#cat-stash-entry .csbtn{display:inline-flex;align-items:center;gap:6px;cursor:pointer;border-radius:6px;padding:2px 4px 2px 0}' +
      '#cat-stash-entry .cstt{font-size:14px;font-weight:430;color:var(--color-token-text-tertiary,rgba(238,241,247,.56))}' +
      '#cat-stash-entry .cscount{font-size:12px;color:var(--color-token-text-tertiary,rgba(238,241,247,.45));font-variant-numeric:tabular-nums}' +
      '#cat-stash-entry .cschev{width:14px;height:14px;opacity:0;transition:opacity .12s ease,transform .15s ease}' +
      '#cat-stash-entry .csbtn:hover .cschev{opacity:1}' +
      '#cat-stash-entry.cscol .cschev{transform:rotate(-90deg)}' +
      '#cat-stash-entry.cscol .csbody{display:none}' +
      '#cat-stash-entry .csbody{padding-top:3px;display:flex;flex-direction:column;gap:3px}' +
      '#cat-stash-entry .csempty{padding:5px 16px;font-size:12.5px;color:var(--color-token-text-tertiary,rgba(238,241,247,.45))}' +
      '#cat-stash-entry .csrow{display:flex;align-items:center;gap:8px;padding:5px 16px}' +
      '#cat-stash-entry .cstext{flex:1 1 auto;min-width:0;font-size:13px;color:var(--color-token-text-primary,#ededed);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}' +
      '#cat-stash-entry .csact{flex:0 0 auto;display:inline-flex;align-items:center;justify-content:center;width:22px;height:22px;border-radius:5px;cursor:pointer;color:var(--color-token-text-secondary,#8c8782);opacity:.75}' +
      '#cat-stash-entry .csact:hover{opacity:1;background:rgba(128,128,128,.16);color:var(--cl-accent,#6c83c4)}' +
      '#cat-stash-entry .csact svg{width:14px;height:14px}' +
      // composer 工具栏按钮组
      '#cat-stash-bar{display:inline-flex;align-items:center;gap:4px;margin-right:4px}' +
      '#cat-stash-bar button{position:relative;display:inline-flex;align-items:center;justify-content:center;width:30px;height:30px;border-radius:8px;border:1px solid var(--color-token-border,rgba(128,128,128,.28));background:transparent;color:var(--color-token-text-secondary,#8c8782);cursor:pointer;padding:0}' +
      '#cat-stash-bar button:hover:not(:disabled){color:var(--cl-accent,#6c83c4);border-color:color-mix(in srgb,var(--cl-accent,#6c83c4) 40%,transparent)}' +
      '#cat-stash-bar button:disabled{opacity:.35;cursor:default}' +
      '#cat-stash-bar button svg{width:16px;height:16px}' +
      '#cat-stash-bar .csbadge{position:absolute;top:-4px;right:-4px;min-width:14px;height:14px;padding:0 3px;border-radius:7px;background:var(--cl-accent,#6c83c4);color:#fff;font-size:9px;line-height:14px;text-align:center;font-variant-numeric:tabular-nums}' +
      // 下拉选单
      '#cat-stash-menu{position:fixed;z-index:99999;min-width:220px;max-width:360px;max-height:300px;overflow-y:auto;padding:4px;border-radius:10px;background:var(--color-token-dropdown-background,#23262e);border:1px solid var(--color-token-border,rgba(128,128,128,.3));box-shadow:0 8px 28px rgba(0,0,0,.4)}' +
      '#cat-stash-menu .csmi{display:flex;align-items:center;gap:8px;padding:7px 9px;border-radius:6px;cursor:pointer;font-size:13px;color:var(--color-token-text-primary,#ededed)}' +
      '#cat-stash-menu .csmi:hover{background:rgba(128,128,128,.16)}' +
      '#cat-stash-menu .csmi .t{flex:1 1 auto;min-width:0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}';
    (document.head || document.documentElement).appendChild(st);
  }

  function el(tag, cls, txt) { var e = document.createElement(tag); if (cls) e.className = cls; if (txt != null) e.textContent = txt; return e; } // textContent sink,杜绝 XSS
  var ICON = {
    push: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v10"/><path d="M8 9l4 4 4-4"/><path d="M4 17v2a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-2"/></svg>',
    pop: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21V11"/><path d="M8 15l4-4 4 4"/><path d="M4 7V5a1 1 0 0 1 1-1h14a1 1 0 0 1 1 1v2"/></svg>',
    restore: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 14l-4-4 4-4"/><path d="M5 10h11a4 4 0 0 1 0 8h-1"/></svg>',
    send: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 2L11 13"/><path d="M22 2l-7 20-4-9-9-4 20-7z"/></svg>',
    del: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2"/><path d="M19 6l-1 14a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1L5 6"/></svg>'
  };
  function iconBtn(cls, ic, title, onClick) {
    var b = el('span', cls); b.innerHTML = ICON[ic]; b.title = title;
    b.addEventListener('click', function (ev) { ev.stopPropagation(); onClick(); });
    return b;
  }

  // ── 折叠态(localStorage 记忆)──
  function isCollapsed() { try { return localStorage.getItem('catStashCollapsed') === '1'; } catch (e) { return false; } }
  function setCollapsed(v) { try { localStorage.setItem('catStashCollapsed', v ? '1' : '0'); } catch (e) {} }

  // ── 面板挂载点(同 quota 的 pinned-summary scroller)──
  // 注意:此选择子与 codex_quota_injector 的 findScroller 完全一致,是 Codex DOM 耦合点(Codex
  // 升级改 section header 标记即失效)。**改这里务必同步改 codex_quota_injector 的同名函数**,否则
  // 升级后会出现「Usage 面板正常、Stash 面板 + 按钮静默消失」的半坏态。
  function findScroller() {
    var btns = document.querySelectorAll('section header button[class~="group/section-toggle"]');
    for (var i = 0; i < btns.length; i++) { var sec = btns[i].closest('section'); if (sec && sec.parentElement) return sec.parentElement; }
    return null;
  }
  var CHEV = '<svg class="cschev" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg>';
  function renderPanel(node) {
    var arr = load();
    node.textContent = '';
    node.classList.toggle('cscol', isCollapsed());
    var h = el('div', 'cshdr');
    var btn = el('span', 'csbtn');
    btn.appendChild(el('span', 'cstt', 'Stash'));
    btn.appendChild(el('span', 'cscount', arr.length ? String(arr.length) : ''));
    var cw = document.createElement('span'); cw.innerHTML = CHEV; if (cw.firstChild) btn.appendChild(cw.firstChild);
    btn.addEventListener('click', function () { var col = !node.classList.contains('cscol'); node.classList.toggle('cscol', col); setCollapsed(col); });
    h.appendChild(btn); node.appendChild(h);
    var body = el('div', 'csbody');
    if (!arr.length) {
      body.appendChild(el('div', 'csempty', '暂无暂存草稿'));
    } else {
      arr.forEach(function (it) {
        var row = el('div', 'csrow');
        var t = el('div', 'cstext', (it.text || '').replace(/\s+/g, ' ').trim()); t.title = it.text || '';
        row.appendChild(t);
        row.appendChild(iconBtn('csact', 'restore', '恢复到输入框', function () { restore(it.id); }));
        row.appendChild(iconBtn('csact', 'send', '直接发送', function () { sendItem(it.id); }));
        row.appendChild(iconBtn('csact', 'del', '删除', function () { del(it.id); }));
        body.appendChild(row);
      });
    }
    node.appendChild(body);
  }
  function ensurePanel() {
    var scroller = findScroller();
    var node = document.getElementById('cat-stash-entry');
    if (!scroller) { if (node) node.remove(); window.__catStashSig = null; return; }
    var fresh = !node;
    if (!node) { node = document.createElement('section'); node.id = 'cat-stash-entry'; }
    // 顺序:始终置于 scroller 末尾(在 quota 之后)。quota injector 配合把自己 insertBefore 到 stash 前。
    if (node.parentElement !== scroller || node !== scroller.lastElementChild) { scroller.appendChild(node); }
    // 仅在数据变化(或新建)时重建 body:renderPanel 做 textContent='' + 重建子节点(childList
    // 变更),本模块 observer 监听 body childList → 若每 tick 无条件重建就是 ~60fps 自循环(空闲也跑)。
    // 用签名 guard 跳过无变化重建(对齐 quota 的 __catQuotaSig)。折叠态由 header 点击直接 toggle class,
    // 不经重建,故不入签名。
    var sig = JSON.stringify(load());
    if (fresh || sig !== window.__catStashSig) { renderPanel(node); window.__catStashSig = sig; }
  }

  // ── 下拉选单(pop 多条时)──
  function closeMenu() { var m = document.getElementById('cat-stash-menu'); if (m) m.remove(); document.removeEventListener('mousedown', onDocDown, true); }
  function onDocDown(ev) { var m = document.getElementById('cat-stash-menu'); if (m && !m.contains(ev.target)) closeMenu(); }
  function openMenu(anchor) {
    closeMenu();
    var arr = load(); if (!arr.length) return;
    var m = el('div'); m.id = 'cat-stash-menu';
    arr.forEach(function (it) {
      var mi = el('div', 'csmi');
      var sp = document.createElement('span'); sp.innerHTML = ICON.restore; sp.style.opacity = '.7'; sp.style.flex = '0 0 auto';
      var svg = sp.firstChild; if (svg) { svg.setAttribute('width', '14'); svg.setAttribute('height', '14'); mi.appendChild(svg); }
      mi.appendChild(el('span', 't', (it.text || '').replace(/\s+/g, ' ').trim()));
      mi.title = it.text || '';
      mi.addEventListener('click', function (ev) { ev.stopPropagation(); restore(it.id); });
      m.appendChild(mi);
    });
    document.body.appendChild(m);
    var r = anchor.getBoundingClientRect();
    // 弹在按钮上方(composer 在底部),右对齐按钮
    var top = Math.max(8, r.top - m.offsetHeight - 6);
    var left = Math.max(8, Math.min(r.right - m.offsetWidth, window.innerWidth - m.offsetWidth - 8));
    m.style.top = top + 'px'; m.style.left = left + 'px';
    setTimeout(function () { document.addEventListener('mousedown', onDocDown, true); }, 0);
  }

  // ── composer 工具栏按钮组(push + pop)──
  function ensureBar() {
    var send = document.querySelector('button[class*="size-token-button-compose"]');
    if (!send || !send.parentElement) { var b = document.getElementById('cat-stash-bar'); if (b) b.remove(); return; }
    var host = send.parentElement;   // div.flex.shrink-0(Dictate + Send 同排,CDP 实证)
    var bar = document.getElementById('cat-stash-bar');
    if (!bar) {
      bar = el('span'); bar.id = 'cat-stash-bar';
      var push = el('button'); push.innerHTML = ICON.push; bar.appendChild(push);
      var pop = el('button'); pop.innerHTML = ICON.pop;
      var badge = el('span', 'csbadge'); pop.appendChild(badge);
      bar.appendChild(pop);
      push.addEventListener('click', function (ev) { ev.preventDefault(); ev.stopPropagation(); stashCurrent(); });
      pop.addEventListener('click', function (ev) {
        ev.preventDefault(); ev.stopPropagation();
        var arr = load(); if (!arr.length) return;
        if (arr.length === 1) restore(arr[0].id); else openMenu(pop);
      });
      bar.__push = push; bar.__pop = pop; bar.__badge = badge;
    }
    if (bar.parentElement !== host || host.firstChild !== bar) { host.insertBefore(bar, host.firstChild); }
    // 状态刷新(仅变化时写,避免自触发 observer churn)
    var arr = load();
    var pushDisabled = !(composerText().trim());
    if (bar.__push.disabled !== pushDisabled) bar.__push.disabled = pushDisabled;
    bar.__push.title = '暂存当前输入';
    var popShow = arr.length > 0 ? '' : 'none';
    if (bar.__pop.style.display !== popShow) bar.__pop.style.display = popShow;
    bar.__pop.title = arr.length > 1 ? ('恢复暂存(' + arr.length + ' 条)') : '恢复暂存';
    var cnt = arr.length > 1 ? String(arr.length) : '';
    if (bar.__badge.textContent !== cnt) bar.__badge.textContent = cnt;
    bar.__badge.style.display = cnt ? '' : 'none';
  }

  function ensure() { try { ensureStyle(); ensurePanel(); ensureBar(); } catch (e) {} }
  window.__catStashEnsure = ensure;

  // rAF 合并的 observer:DOM 变更(含 composer 文本变化 / React re-render 驱逐注入节点)即 re-ensure。
  var scheduled = false;
  var mo = new MutationObserver(function () { if (scheduled) return; scheduled = true; requestAnimationFrame(function () { scheduled = false; ensure(); }); });
  mo.observe(document.body, { childList: true, subtree: true, characterData: true });
  window.__catStashObserver = mo;
  window.__catStashInstalled = true;
  ensure();
})();
"##;

/// 卸载脚本:断 observer、删注入 DOM(面板/工具栏按钮/下拉/style)、清全局态。幂等。
const REMOVE_SCRIPT: &str = r#"
(function() {
  try { if (window.__catStashObserver) window.__catStashObserver.disconnect(); } catch (e) {}
  ['cat-stash-entry', 'cat-stash-bar', 'cat-stash-menu', 'cat-stash-style'].forEach(function(id){ var n = document.getElementById(id); if (n) n.remove(); });
  try {
    delete window.__catStashInstalled; delete window.__catStashVersion;
    delete window.__catStashObserver; delete window.__catStashEnsure; delete window.__catStashSig;
  } catch (e) {}
})();
"#;

/// 读 registry config 的 `settings.codexStashEnabled`(默认关)。对齐 quota 的 `quota_enabled`。
fn stash_enabled() -> bool {
    crate::admin::registry_io::load()
        .ok()
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("codexStashEnabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// CDP push 失败分类(对齐 quota injector 的 `PushError`):
/// - `Connect`:Codex 没跑 / 端口未就绪 = 注入还连不上,**常态**,不告警。
/// - `Evaluate`:已连上但发送 / 注入 JS 抛错(选择子随 Codex 升级失效等)= 真异常,warn-once。
enum StashPushError {
    Connect(String),
    Evaluate(String),
}

/// 一次 CDP `Runtime.evaluate`。复用 [`crate::codex_theme_injector`] 的共享 CDP helper。
async fn evaluate_once(script: &str) -> Result<(), StashPushError> {
    let ws_url = locate_main_window_ws()
        .await
        .map_err(|e| StashPushError::Connect(e.to_string()))?;
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .map_err(|e| StashPushError::Connect(e.to_string()))?;
    let (mut write, mut read) = ws_stream.split();
    let (msg, _) = make_msg(
        1,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
    );
    write
        .send(WsMessage::Text(msg.into()))
        .await
        .map_err(|e| StashPushError::Evaluate(e.to_string()))?;
    drain_until_response(&mut read, 1)
        .await
        .map_err(StashPushError::Evaluate)?;
    let _ = write.close().await;
    Ok(())
}

/// Stash 注入 daemon:`main.rs` spawn 一次,独立于 quota 开关。每 tick 推幂等 install(纯保活,
/// 覆盖 Codex reload/导航);关闭时推一次 remove 清残留(成功才复位 needs_remove)。
/// CDP 不可达(Codex 没跑 / 端口未就绪)→ 静默跳过本 tick(debug 级,常态)。
pub async fn run_stash_daemon() {
    const TICK: Duration = Duration::from_secs(5);
    let mut needs_remove = true;
    let mut evaluate_warned = false;
    loop {
        tokio::time::sleep(TICK).await;
        if stash_enabled() {
            needs_remove = true;
            match evaluate_once(INSTALL_SCRIPT).await {
                Ok(()) => evaluate_warned = false,
                // Connect 失败 = Codex 没跑 / 端口未就绪,常态,只 debug(默认不输出)不刷屏。
                Err(StashPushError::Connect(e)) => {
                    tracing::debug!(error = %e, "[Stash] CDP 未就绪(Codex 没跑 / 端口未就绪),常态");
                }
                // Evaluate 失败 = Codex 在跑但注入异常(选择子失效等),warn-once,便于定位
                // 「Codex 没启动」vs「注入坏了」(refute 这条会丢掉 quota 早就有的诊断信号)。
                Err(StashPushError::Evaluate(e)) => {
                    if !evaluate_warned {
                        tracing::warn!(error = %e, "[Stash] 注入 evaluate 失败 — Codex 在跑但注入异常(选择子可能随 Codex 升级失效)");
                        evaluate_warned = true;
                    }
                }
            }
        } else if needs_remove {
            match evaluate_once(REMOVE_SCRIPT).await {
                Ok(()) => needs_remove = false,
                Err(_) => {} // CDP 瞬时不可达 → 保持 needs_remove,下 tick 重试
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_has_idempotent_guard() {
        assert!(INSTALL_SCRIPT.contains("window.__catStashInstalled"));
        assert!(INSTALL_SCRIPT.contains("var VERSION ="));
    }

    #[test]
    fn remove_script_clears_nodes_and_globals() {
        assert!(REMOVE_SCRIPT.contains("cat-stash-entry"));
        assert!(REMOVE_SCRIPT.contains("cat-stash-bar"));
        assert!(REMOVE_SCRIPT.contains("delete window.__catStashInstalled"));
    }

    #[test]
    fn raw_scripts_have_no_terminator_collision() {
        // r##"..."## / r#"..."# 定界符不能在脚本体里出现
        assert!(!INSTALL_SCRIPT.contains("\"##"));
        assert!(!REMOVE_SCRIPT.contains("\"#"));
    }
}
