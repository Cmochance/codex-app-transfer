//! Codex Desktop pinned summary 弹窗「Usage」用量条目注入器(MOC-204)。
//!
//! 在 pinned summary 弹窗(顶栏 "Toggle pinned summary",内含 Environment /
//! Sources 等 `<section>`)底部注入一个独立 `Usage` section,展示最多 4 行:
//! 5 小时额度、每周额度(上游 rate limit,仅白名单 provider)+ 上下文、Tokens
//! 速率/累计(本地可算,全 provider)。每行可为带进度条的 `bar` 或纯统计的
//! `stat`,由 push 的结构化 payload 描述。
//!
//! **已完整实现(Phase 1-3)**:daemon 调 [`build_payload`] 推实际数据——
//! Phase 2 本地 usage(上下文 fiber 读取 / Tokens 速率 / 缓存命中率)+ Phase 3
//! 白名单 provider 真实额度(`fetch_antigravity_quota`)均已上线;
//! [`build_mock_payload`] 仅保留供测试锁 JS↔payload 结构契约。
//!
//! **架构:纯周期推送,无 `Page.addScriptToEvaluateOnNewDocument` 注册**:
//! - 页面 CSP `connect-src` 只允许 chatgpt 系域名,注入 JS 无法 fetch 本地
//!   端口拉数据 → 只能 Rust 侧推。
//! - daemon 每 tick 通过 CDP `Runtime.evaluate` 执行「幂等 install + update」
//!   组合脚本:页面 reload / Codex 重启丢状态后,下一 tick 自动重装,无需
//!   维护注册 identifier(也规避 theme 那种"注册无法撤销"的复发问题)。
//! - DOM 锚定:弹窗 scroller(`section > header > button.group/section-toggle`
//!   的父容器,CDP 实测 v26.608)末尾 append 自有 `<section id=cat-quota-entry>`;
//!   MutationObserver 守护 React re-render 拆节点后重挂。Tailwind 命名组 class
//!   是源码字面名,不随构建 hash 变,跟 theme injector 的 selector 同一脆弱级别
//!   (Codex 升级需回归)。
//!
//! 开关:transfer settings `codexQuotaEnabled`(默认关)。daemon 每 tick 读
//! settings,关闭时推一次 remove 脚本清掉 DOM 后静默。

use serde_json::json;

use futures::{SinkExt, StreamExt};
use tokio::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::codex_theme_injector::{drain_until_response, locate_main_window_ws, make_msg};
use crate::provider_quota::{ProviderQuota, QuotaWindow};

/// 幂等安装脚本:注入 scoped `<style>` + 定义 `window.__catQuotaUpdate` +
/// MutationObserver 守护。已安装(`__catQuotaInstalled`)时直接跳过,daemon
/// 每 tick 重发零副作用。
///
/// payload schema(`__catQuotaUpdate(data)`):
/// ```json
/// { "header": "Usage", "title": "<hover tooltip>",
///   "rows": [
///     {"kind":"bar","cls":"quota|local","label":"5 小时额度","pct":6,"detail":"6% · 06-13 11:32 刷新"},
///     {"kind":"stat","label":"Tokens","detail":"78 tok/s · 累计 128.5k"}
///   ] }
/// ```
/// 视觉对齐 Codex 原生 section:`Usage` 标题可折叠(chevron + localStorage 记忆),
/// 行标签用内容区常规字重/字号,进度条颜色取主题注入的 `--cl-accent` /
/// `--cl-accent-soft`(随壁纸主题变,见 codex_theme_injector)。文本一律走
/// `textContent` sink、宽度数值 clamp 0-100,不可信串经 serde_json 转义无 XSS。
const INSTALL_SCRIPT: &str = r##"
(function() {
  // [MOC-230] 版本化幂等 guard:同版本跳过(常态);版本变(应用升级后 INSTALL_SCRIPT 改了)
  // → 先拆旧 observer + DOM 节点再重装,使新逻辑无需重启 Codex 即覆盖旧注入。旧版只有
  // __catQuotaInstalled、无 __catQuotaVersion(undefined ≠ 当前版本)→ 同样触发重装。
  var VERSION = 3; // [MOC-231] 加 breakdown caret/下拉 + convId 守卫 → bump,升级后免重启 Codex 即覆盖旧注入
  if (window.__catQuotaInstalled) {
    if (window.__catQuotaVersion === VERSION) return;
    try { if (window.__catQuotaObserver) window.__catQuotaObserver.disconnect(); } catch (e) {}
    var __stale = document.getElementById('cat-quota-entry');
    if (__stale) __stale.remove();
  }
  window.__catQuotaVersion = VERSION;
  window.__catQuotaLast = null;
  window.__catQuotaSig = null;

  function isCollapsed() {
    try { return localStorage.getItem('catQuotaCollapsed') === '1'; } catch (e) { return false; }
  }
  function setCollapsed(v) {
    try { localStorage.setItem('catQuotaCollapsed', v ? '1' : '0'); } catch (e) {}
  }

  function ensureStyle() {
    if (document.getElementById('cat-quota-style')) return;
    var st = document.createElement('style');
    st.id = 'cat-quota-style';
    st.textContent =
      '#cat-quota-entry{display:block;padding:0 0 6px;user-select:none}' +
      // 标题栏:1:1 复刻 Codex 原生 section header(CDP 实测)—— 常驻深色带(bg token,
      // 跟随主题)、h28、ps-4/pe-2.5/pb-0.5、不随 hover 变色
      '#cat-quota-entry .cqhdr{display:flex;align-items:center;height:28px;padding:0 10px 2px 16px;background:var(--color-token-dropdown-background,rgba(20,24,36,.78))}' +
      // 标题+箭头 = 内联可点组(对应原生 button.group/section-toggle):仅 hover 显箭头
      '#cat-quota-entry .cqbtn{display:inline-flex;align-items:center;gap:6px;cursor:pointer;border-radius:6px;padding:2px 4px 2px 0}' +
      '#cat-quota-entry .cqtt{font-size:14px;font-weight:430;color:var(--color-token-text-tertiary,rgba(238,241,247,.56))}' +
      '#cat-quota-entry .cqchev{width:14px;height:14px;opacity:0;transition:opacity .12s ease,transform .15s ease}' +
      '#cat-quota-entry .cqbtn:hover .cqchev{opacity:1}' +
      '#cat-quota-entry.cqcol .cqchev{transform:rotate(-90deg)}' +
      '#cat-quota-entry.cqcol .cqbody{display:none}' +
      '#cat-quota-entry .cqbody{padding-top:3px}' +
      '#cat-quota-entry .cqb{padding:5px 16px;display:flex;flex-direction:column;gap:5px}' +
      '#cat-quota-entry .cqb .cqt{display:flex;align-items:center;justify-content:space-between;gap:10px}' +
      // 行标签:内容区常规字重/字号(不大不粗),跟随主题 ink 主色
      '#cat-quota-entry .cql{font-size:13.5px;font-weight:400;color:var(--color-token-text-primary,#ededed)}' +
      '#cat-quota-entry .cqd{font-size:12.5px;color:var(--color-token-text-secondary,#8c8782);font-variant-numeric:tabular-nums;white-space:nowrap}' +
      '#cat-quota-entry .cqk{height:5px;border-radius:3px;background:rgba(128,128,128,.22);overflow:hidden}' +
      // 进度条:开壁纸主题 → 取注入的 --cl-accent(随壁纸调);没开主题 → 回退贴合
      // Codex 暗色原生 UI 的中性蓝(不用暖色,避免跟原生蓝调撞色)
      '#cat-quota-entry .cqk>i{display:block;height:100%;border-radius:3px;background:var(--cl-accent,#6c83c4)}' +
      '#cat-quota-entry .cqb.local .cqk>i{background:var(--cl-accent-soft,#9aa9d8)}' +
      '#cat-quota-entry .cqb.hot .cqk>i{background:#e8606a}' +
      // Tokens + 缓存命中率合并行:左 token 速率/累计(主色)· 右 缓存命中(次级)
      '#cat-quota-entry .cqduo{display:flex;align-items:center;justify-content:space-between;gap:10px;padding:7px 16px;font-variant-numeric:tabular-nums}' +
      '#cat-quota-entry .cqduo .l{font-size:13px;color:var(--color-token-text-primary,#ededed)}' +
      '#cat-quota-entry .cqduo .r{font-size:13px;color:var(--color-token-text-secondary,#8c8782);white-space:nowrap}' +
      '#cat-quota-entry .cqs{display:flex;align-items:center;justify-content:space-between;gap:10px;padding:7px 16px}' +
      // [MOC-231] 上下文明细:detail + caret 右对齐成组(间距 5px),点开内联展开 by-source 下拉
      '#cat-quota-entry .cqctxr{display:flex;align-items:center;gap:5px}' +
      '#cat-quota-entry .cqbdcaret{display:inline-flex;align-items:center;cursor:pointer;opacity:.5;transition:transform .15s ease}' +
      '#cat-quota-entry .cqbdcaret:hover{opacity:.9}' +
      '#cat-quota-entry .cqb.cqbdopen .cqbdcaret{transform:rotate(180deg)}' +
      '#cat-quota-entry .cqbd{display:none;flex-direction:column;gap:4px;margin-top:7px}' +
      '#cat-quota-entry .cqb.cqbdopen .cqbd{display:flex}' +
      '#cat-quota-entry .cqbdrow{display:flex;align-items:center;gap:8px;font-size:12px}' +
      '#cat-quota-entry .cqbdsw{width:9px;height:9px;border-radius:2px;flex:0 0 auto}' +
      '#cat-quota-entry .cqbdlb{flex:1 1 auto;color:var(--color-token-text-primary,#ededed);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}' +
      '#cat-quota-entry .cqbdtk{color:var(--color-token-text-secondary,#8c8782);font-variant-numeric:tabular-nums}' +
      '#cat-quota-entry .cqbdpc{color:var(--color-token-text-tertiary,rgba(238,241,247,.5));width:48px;text-align:right;font-variant-numeric:tabular-nums}' +
      // [MOC-231] payload 非当前活动会话时,隐藏 breakdown caret + 下拉(对齐 refreshDuo 的 uuid 守卫,防切对话 1 tick stale 串显)
      '#cat-quota-entry .cqb.cqbdmismatch .cqbdcaret{display:none}' +
      '#cat-quota-entry .cqb.cqbdmismatch .cqbd{display:none}';
    (document.head || document.documentElement).appendChild(st);
  }

  function findScroller() {
    // pinned summary 弹窗里带 section-toggle header 的 section 们(Environment /
    // Sources …),它们的父容器(scroller)是注入挂载点。class 用属性包含匹配,
    // 避免 "group/section-toggle" 里的斜杠转义问题。
    var btns = document.querySelectorAll('section header button[class~="group/section-toggle"]');
    for (var i = 0; i < btns.length; i++) {
      var sec = btns[i].closest('section');
      if (sec && sec.parentElement) return sec.parentElement;
    }
    return null;
  }

  function el(tag, cls, txt) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (txt != null) e.textContent = txt;   // textContent sink,杜绝 HTML 注入
    return e;
  }

  // chevron 是静态 SVG(无用户数据),innerHTML 安全
  var CHEV = '<svg class="cqchev" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg>';

  function buildHeader(node, data) {
    var h = el('div', 'cqhdr');
    var btn = el('span', 'cqbtn');   // 对应原生 button.group/section-toggle(内联可点组)
    btn.appendChild(el('span', 'cqtt', data.header || 'Usage'));
    var cw = document.createElement('span');
    cw.innerHTML = CHEV;
    if (cw.firstChild) btn.appendChild(cw.firstChild);
    btn.addEventListener('click', function() {
      var col = !node.classList.contains('cqcol');
      node.classList.toggle('cqcol', col);
      setCollapsed(col);
    });
    h.appendChild(btn);
    return h;
  }

  // Codex 自己的上下文 %:composer 旁环图标的 aria-label(CDP 实测「Context usage: 32%」
  // 挂在 aria-label,非 title),常驻 DOM、不需 hover/对话触发;title 作兜底。绝对值
  // 「62k / 190k」只在 hover tooltip(平时不在 DOM),故只读 %。
  function readCtxPct() {
    var e = document.querySelector('[aria-label^="Context usage:"], [title^="Context usage:"]');
    if (!e) return null;
    var s = e.getAttribute('aria-label') || e.getAttribute('title') || '';
    var m = s.match(/(\d+(?:\.\d+)?)\s*%/);
    if (!m) return null;
    var v = parseFloat(m[1]);
    return isNaN(v) ? null : Math.max(0, Math.min(100, v));
  }
  // token 数格式化:≥1M(含会被 round 到 1000k 的)显 M(整数不带小数,如 1M / 1.5M),
  // 否则整数 k(对齐 Codex 文案,如 62k / 200k)。修「1M 模型显 1000k」。
  function fmtTok(n) {
    if (n >= 9.995e8) { // ≥~1000M → B(十亿),防「1000M」过长
      var b = n / 1e9;
      return (Math.abs(b - Math.round(b)) < 0.05 ? Math.round(b) : b.toFixed(1)) + 'B';
    }
    if (n >= 999500) {
      var m = n / 1e6;
      return (Math.abs(m - Math.round(m)) < 0.05 ? Math.round(m) : m.toFixed(1)) + 'M';
    }
    return Math.round(n / 1e3) + 'k';
  }
  // 从 Codex 的 React fiber 直接读「已有对话」的上下文用量,重启/恢复即有值、不需发
  // 新对话(CDP 实证 v26.609:从环元素向上爬 fiber,memoizedProps 里有 contextUsage =
  // {percent, usedTokens, contextWindow, remainingTokens})。键名变了就 return null 退回
  // aria %(优雅降级,不抛)。
  function readCtxUsage() {
    try {
      var ring = document.querySelector('[aria-label^="Context usage:"]');
      if (!ring) return null;
      var fkey = null;
      for (var k in ring) { if (k.indexOf('__reactFiber$') === 0) { fkey = k; break; } }
      if (!fkey) return null;
      var f = ring[fkey], n = 0;
      while (f && n < 25) {
        var bags = [f.memoizedProps, f.memoizedState];
        for (var b = 0; b < bags.length; b++) {
          var bag = bags[b];
          if (bag && typeof bag === 'object') {
            for (var key in bag) {
              var v = bag[key];
              if (v && typeof v === 'object' &&
                  typeof v.usedTokens === 'number' && typeof v.contextWindow === 'number') {
                return { used: v.usedTokens, effWin: v.contextWindow };
              }
            }
          }
        }
        f = f.return; n++;
      }
    } catch (e) {}
    return null;
  }
  // MOC-230 对话隔离:从 React fiber 读当前活动会话 conversationId(== rollout 文件名 uuid,
  // == session_meta payload.id,2026-06-14 解包+真机实证)。多锚点(上下文环 / pinned summary
  // scroller / 面板所在 section)向上爬 props/state 找 conversationId(uuid 形态)。读不到 →
  // null:daemon 据此 fail-closed 不显累计/缓存,绝不串到别的对话。
  var __CONVID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
  function convIdFromFiber(start) {
    if (!start) return null;
    var fkey = null;
    for (var k in start) { if (k.indexOf('__reactFiber$') === 0) { fkey = k; break; } }
    if (!fkey) return null;
    var f = start[fkey], n = 0;
    while (f && n < 40) {
      var bags = [f.memoizedProps, f.memoizedState];
      for (var b = 0; b < bags.length; b++) {
        var bag = bags[b];
        if (bag && typeof bag === 'object') {
          for (var key in bag) {
            if (key === 'conversationId' || /[Cc]onversationId$/.test(key)) {
              var v = bag[key];
              if (typeof v === 'string' && __CONVID_RE.test(v)) return v;
            }
          }
        }
      }
      f = f.return; n++;
    }
    return null;
  }
  function readConvId() {
    try {
      var ctxSection = document.querySelector('#cat-quota-entry');
      var anchors = [
        document.querySelector('[aria-label^="Context usage:"]'),
        findScroller(),
        ctxSection ? ctxSection.previousElementSibling : null,
      ];
      for (var i = 0; i < anchors.length; i++) {
        var id = convIdFromFiber(anchors[i]);
        if (id) return id;
      }
    } catch (e) {}
    return null;
  }
  window.__catActiveConvId = readConvId;
  // [MOC-231] per-conv 用量缓存(localStorage,Codex 侧 → Codex/transfer 重启都活、按 cid 不串):
  // 上下文绝对值(fiber)/ 累计 / 缓存命中 就绪时缓存;(re)load 后这些 live 源没就绪前先显本
  // 对话缓存的上次值,做到「启动即显」。% 仍优先 aria 实时(它本来就即时)。
  function usageCacheGet(cid) {
    if (!cid) return null;
    try {
      var s = localStorage.getItem('catUsageCache:' + cid);
      return s ? JSON.parse(s) : null;
    } catch (e) { return null; }
  }
  function usageCacheMerge(cid, patch) {
    if (!cid) return;
    try {
      var cur = usageCacheGet(cid) || {};
      for (var k in patch) cur[k] = patch[k];
      localStorage.setItem('catUsageCache:' + cid, JSON.stringify(cur));
    } catch (e) {}
  }
  // 刷新上下文行(每次 ensureNode,含 observer 触发 → live):优先 fiber 精确值(立即、
  // 不需对话);读不到退回 Codex aria 的 %(只有 %)。
  function refreshContext(node, cid) {
    var w = node && node.querySelector('[data-ctx]');
    if (!w) return;
    var d = w.querySelector('.cqctxd');
    var fill = w.querySelector('.cqctxfill');
    var pct, detail;
    var u = readCtxUsage();
    if (u && u.effWin > 0) {
      // 满窗口 = 有效窗口 ÷ 0.95(把 Codex 扣的 5% reserve 加回来显示真实上限);
      // used / 满窗口 = 真实占比(故比 Codex 环的 % 略低)。整数 k 对齐 Codex 文案格式。
      var fullWin = Math.round(u.effWin / 0.95);
      pct = Math.max(0, Math.min(100, (u.used / fullWin) * 100));
      detail = fmtTok(u.used) + ' / ' + fmtTok(fullWin) + ' · ' + Math.round(pct) + '%';
      // [MOC-231] 缓存绝对值:下次 (re)load fiber 没就绪前用它即显「X / Y」,不再只剩 %。
      usageCacheMerge(cid, { ctxUsed: u.used, ctxWin: fullWin });
    } else {
      // fiber 没就绪(刚 (re)load):优先本对话缓存的绝对值即显(% 用 aria 实时,无则按缓存算);
      // 无缓存才退回 aria 的「只有 %」。
      var aria = readCtxPct();
      var c = usageCacheGet(cid);
      if (c && typeof c.ctxUsed === 'number' && c.ctxWin) {
        pct = aria != null ? aria : Math.max(0, Math.min(100, (c.ctxUsed / c.ctxWin) * 100));
        detail = fmtTok(c.ctxUsed) + ' / ' + fmtTok(c.ctxWin) + ' · ' + Math.round(pct) + '%';
      } else {
        pct = aria == null ? 0 : aria;
        detail = aria == null ? '—' : Math.round(aria) + '%';
      }
    }
    // 值相等才不写:textContent/style 赋值会产生 DOM 变更 → 触发本 observer → 若每次都
    // 写就自循环 ~60fps(空闲也跑,code-review IMPORTANT-1)。仅变化时写,稳定即静默。
    var wpx = pct + '%';
    if (d && d.textContent !== detail) d.textContent = detail;
    if (fill && fill.style.width !== wpx) fill.style.width = wpx;
    var hot = pct >= 90;
    if (w.classList.contains('hot') !== hot) w.classList.toggle('hot', hot);
    // [MOC-231] convId 守卫(对齐 refreshDuo):payload 的 breakdown 属于 data.convId 这条对话,
    // 与当前活动 cid 不符时(切对话后 daemon 1 tick 延迟)隐藏 caret + 下拉,绝不把上一对话
    // 的明细串显在新对话下。仅变化时 toggle,稳定不写,避免自触发 observer churn。
    var data = window.__catQuotaLast;
    var bdMismatch = !(data && data.convId != null && data.convId === cid);
    if (w.classList.contains('cqbdmismatch') !== bdMismatch) {
      w.classList.toggle('cqbdmismatch', bdMismatch);
    }
  }

  // ── 实时 tokens/s(MOC-204 §3,参考 Codex 老版 + OpenCode 插件)──
  // SSE 无逐 delta token 数,实时只能按 Codex 流式文本增长估(中文≈0.6 tok/字、其余
  // ≈1/4);2s 滑窗算速率;流停(最近样本 >1.5s)冻结在最后值(用户:保留);没数据 0。
  var __tpsBuf = [];
  var __tpsLast = 0;
  var __tpsConvId = null; // 当前速率归属的会话(MOC-230:切对话即清零重算)
  function inPanel(node) {
    var n = node && node.nodeType === 3 ? node.parentNode : node;
    return !!(n && n.closest && n.closest('#cat-quota-entry'));
  }
  function estTok(s) {
    if (!s) return 0;
    var cjk = (s.match(/[　-鿿가-힯＀-￯]/g) || []).length;
    return cjk * 0.6 + (s.length - cjk) / 4;
  }
  function accumulateTps(muts) {
    var tok = 0;
    for (var i = 0; i < muts.length; i++) {
      var m = muts[i];
      if (inPanel(m.target)) continue;
      if (m.type === 'characterData') {
        var nv = m.target.data || '', ov = m.oldValue || '';
        if (nv.length > ov.length) tok += estTok(nv.indexOf(ov) === 0 ? nv.slice(ov.length) : nv);
      } else if (m.type === 'childList') {
        for (var j = 0; j < m.addedNodes.length; j++) {
          var an = m.addedNodes[j];
          if (!inPanel(an)) tok += estTok(an.textContent || '');
        }
      }
    }
    if (tok > 0) {
      // 批量挂载(切对话 / 渲染历史 / 开 pinned summary)会在一次 observer 回调里涌入整段
      // 历史文本,远超真实流式的每帧增量(rAF 合批下即便快模型单批也就几~几十 token)。整批
      // 超阈值判定为挂载、丢弃不计,避免污染速率 buffer + 被 currentTps 冻结成假速率(review
      // P2:non-stream DOM mounts)。正常流式单批增量极小,永不触阈。注:这是启发式拦截,
      // 真正按「活动 assistant 流」精确隔离属对话隔离 followup(MOC-204 后续)。
      if (tok > 200) return;
      var now = Date.now();
      __tpsBuf.push({ t: now, k: tok });
      while (__tpsBuf.length && now - __tpsBuf[0].t > 2000) __tpsBuf.shift();
    }
  }
  function currentTps() {
    var now = Date.now();
    if (__tpsBuf.length && now - __tpsBuf[__tpsBuf.length - 1].t > 1500) __tpsBuf.length = 0; // 流停冻结
    while (__tpsBuf.length && now - __tpsBuf[0].t > 2000) __tpsBuf.shift();
    if (!__tpsBuf.length) return __tpsLast;
    var sum = 0;
    for (var i = 0; i < __tpsBuf.length; i++) sum += __tpsBuf[i].k;
    if (sum < 3) return __tpsLast; // 低于阈值=时钟等噪声,当空闲
    var span = Math.max(0.3, (now - __tpsBuf[0].t) / 1000);
    __tpsLast = Math.round(sum / span);
    return __tpsLast;
  }
  function refreshTps(node, cid) {
    // 对话隔离(MOC-230):活动 conversationId 变了(切对话)→ 清空速率 buffer,新对话
    // 从零重估,不把上个对话的速率带过来。仅在 cid **非空且变化**时重置:readConvId 偶发
    // 读不到(返 null)时不误清正在累积的速率(读不到 ≠ 切对话)。
    if (cid && cid !== __tpsConvId) {
      __tpsBuf.length = 0;
      __tpsLast = 0;
      __tpsConvId = cid;
    }
    var s = node && node.querySelector('.cqrate');
    if (!s) return;
    // 同 refreshContext:仅变化时写,避免自触发 observer 的 ~60fps 空转(流式时值在变 →
    // 正常实时刷;空闲时 currentTps 返回冻结值不变 → 不写 → observer 不被自身唤醒)。
    // cid 读不到(null)→ 无法确认速率归属哪个对话 → 显 0(fail-closed,绝不显上个对话的
    // 冻结速率;与 cum/cache 的 null→「—」一致,review P2)。不清 buffer:transient null
    // 不丢已累积样本,cid 恢复(仍同对话)即继续显实时速率。
    var t = (cid ? currentTps() : 0) + ' token/s';
    if (s.textContent !== t) s.textContent = t;
  }
  // 刷新累计/缓存命中(MOC-230 对话隔离):payload 标注的 convId 与当前活动 conversationId
  // 一致才显 daemon 算的值;不一致(切对话、daemon 还没追上)→ 显「—」,绝不显别的对话数据。
  function refreshDuo(node, cid) {
    var cumEl = node.querySelector('.cqcum');
    var cacheEl = node.querySelector('.cqcache');
    if (!cumEl && !cacheEl) return;
    var data = window.__catQuotaLast;
    var duo = null;
    if (data) {
      var rows = data.rows || [];
      for (var i = 0; i < rows.length; i++) { if (rows[i] && rows[i].kind === 'duo') { duo = rows[i]; break; } }
    }
    var match = !!(data && duo && data.convId != null && data.convId === cid);
    var cumV, cacheV;
    if (match) {
      // payload 就是本对话:用 live 值,并缓存供下次 (re)load 即显(占位「—」不缓存)。
      cumV = duo.cum || '累计 —';
      cacheV = duo.right || '缓存命中 —';
      var patch = {};
      if (cumV.indexOf('—') < 0) patch.cum = cumV;
      if (cacheV.indexOf('—') < 0) patch.cacheRight = cacheV;
      usageCacheMerge(cid, patch);
    } else {
      // [MOC-231] payload 还不是本对话(daemon 1-tick / 冷启动还没 push 到本对话):
      // 显本对话缓存的上次值(按 cid 取,绝不串别的对话);无缓存才「—」。
      var c = usageCacheGet(cid);
      cumV = (c && c.cum) || '累计 —';
      cacheV = (c && c.cacheRight) || '缓存命中 —';
    }
    if (cumEl && cumEl.textContent !== cumV) cumEl.textContent = cumV;
    if (cacheEl && cacheEl.textContent !== cacheV) cacheEl.textContent = cacheV;
  }

  // [MOC-231] by-source 明细:分类 key → 中文 label(与后端 ContextBreakdown.categories[].key 对齐)
  var BD_LABELS = {
    tool_calls: '工具调用与输出', messages: '对话消息', reasoning: '推理',
    developer: '开发者指令', tools: '工具定义', system_prompt: '系统提示'
  };
  var BDCHEV = '<svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg>';
  function ctxBdOpen() { try { return localStorage.getItem('catCtxBdOpen') === '1'; } catch (e) { return false; } }
  function setCtxBdOpen(v) { try { localStorage.setItem('catCtxBdOpen', v ? '1' : '0'); } catch (e) {} }
  function fmtTokFine(n) {
    n = n || 0;
    if (n >= 1e9) return (n / 1e9).toFixed(2).replace(/\.?0+$/, '') + 'B';
    if (n >= 1e6) return (n / 1e6).toFixed(2).replace(/\.?0+$/, '') + 'M';
    if (n >= 1000) return (n / 1000).toFixed(1).replace(/\.0$/, '') + 'k';
    return String(Math.round(n));
  }
  // 小方块统一用注入主题的 accent 色系(--cl-accent),从上往下(占比降序)随占比减小逐渐变浅
  // —— 同色系不同深度,用降不透明度实现(top 最深、bottom 最浅)。
  function swatchOpacity(i, n) { return n > 1 ? (1 - i * (0.62 / (n - 1))) : 1; }
  // 百分比按占「总上下文窗口」(非占已用):优先读 Codex fiber 满窗口(effWin/0.95,与 ctx bar
  // 同口径),读不到退回 breakdown 自身总数。<10% 显 1 位小数,>=10% 显整数。
  function ctxPctStr(tokens, denom) {
    var p = 100 * (tokens || 0) / (denom || 1);
    return (p < 10 ? p.toFixed(1) : String(Math.round(p))) + '%';
  }
  // 构造 by-source 明细块(每类一行:色块 + label + tokens + 占总窗口 %)。仿 Claude 上下文下拉。
  function buildBreakdown(bd) {
    var box = el('div', 'cqbd');
    var cats = (bd && bd.categories) || [];
    var n = cats.length;
    var bdTotal = (bd && bd.total_tokens) || cats.reduce(function(s, c) { return s + (c.tokens || 0); }, 0) || 1;
    var u = readCtxUsage();
    // [MOC-231] o200k 口径校准:breakdown 用 o200k(GPT tokenizer)逐 item 算,但活动 provider
    // 的 tokenizer 可能不同(如 gemini/antigravity 对中文 + 跨模型累积历史计数差很多)→ 直接显
    // o200k 总数会和上下文 bar(Codex 权威 used,按真实模型口径)对不上。用 used / o200k总 缩放:
    // 只保留 o200k 的「各类占比」,绝对值 + % 对齐 bar(每次 payload 刷新时对齐,与底层 tokenizer 无关)。
    // fiber 没就绪 / used 读不到或为 0(刚 (re)load 或新对话中途态)→ 退回 o200k 原值(降级,
    // 不缩放),避免 scale=0 把所有分类清零成「全 0」。
    var hasWin = u && u.effWin > 0;
    var fullWin = hasWin ? Math.round(u.effWin / 0.95) : bdTotal;
    var scale = hasWin && u.used > 0 && bdTotal > 0 ? (u.used / bdTotal) : 1;
    cats.forEach(function(c, i) {
      var tok = (c.tokens || 0) * scale; // 缩放到 Codex 权威 used 口径
      var row = el('div', 'cqbdrow');
      var sw = el('span', 'cqbdsw');
      sw.style.background = 'var(--cl-accent,#6c83c4)';
      sw.style.opacity = swatchOpacity(i, n).toFixed(3);
      row.appendChild(sw);
      row.appendChild(el('span', 'cqbdlb', BD_LABELS[c.key] || c.key));
      row.appendChild(el('span', 'cqbdtk', fmtTokFine(tok)));
      row.appendChild(el('span', 'cqbdpc', ctxPctStr(tok, fullWin)));
      box.appendChild(row);
    });
    return box;
  }

  function buildRow(r) {
    if (r && r.kind === 'duo') {
      // 左:实时速率(.cqrate,JS 刷)· 累计(payload);右:缓存命中(payload)
      var d = el('div', 'cqduo');
      var lw = el('span', 'l');
      lw.appendChild(el('span', 'cqrate', '0 token/s'));
      lw.appendChild(document.createTextNode(' · '));
      lw.appendChild(el('span', 'cqcum', r.cum || '累计 —'));
      d.appendChild(lw);
      d.appendChild(el('span', 'r cqcache', r.right || ''));
      return d;
    }
    if (r && r.kind === 'ctx') {
      // 上下文 bar:结构同 local bar,值由 refreshContext 从 Codex 实时填(不靠 payload)
      var cw = el('div', 'cqb local');
      cw.setAttribute('data-ctx', '1'); // 值由 refreshContext 从 Codex fiber 实时填
      var ctop = el('div', 'cqt');
      ctop.appendChild(el('span', 'cql', r.label || '上下文'));
      // [MOC-231] detail(28k/1M·3%)+ caret 右对齐成组,紧贴(间距 5px),不再被 space-between 居中。
      // caret 仿 Claude `⌄`,点开内联展开 breakdown;开合态 localStorage 记忆,renderInto 重建复原。
      var cright = el('span', 'cqctxr');
      cright.appendChild(el('span', 'cqd cqctxd', '—'));
      var bd = r.breakdown;
      var hasBd = !!(bd && bd.categories && bd.categories.length);
      if (hasBd) {
        if (ctxBdOpen()) cw.classList.add('cqbdopen');
        var caret = el('span', 'cqbdcaret');
        caret.innerHTML = BDCHEV;
        caret.title = '上下文明细';
        caret.addEventListener('click', function(ev) {
          ev.stopPropagation();
          var open = !cw.classList.contains('cqbdopen');
          cw.classList.toggle('cqbdopen', open);
          setCtxBdOpen(open);
        });
        cright.appendChild(caret);
      }
      ctop.appendChild(cright);
      cw.appendChild(ctop);
      var ctrack = el('div', 'cqk');
      var cfill = el('i');
      cfill.className = 'cqctxfill';
      ctrack.appendChild(cfill);
      cw.appendChild(ctrack);
      if (hasBd) cw.appendChild(buildBreakdown(bd));
      return cw;
    }
    if (r && r.kind === 'bar') {
      var pct = Math.max(0, Math.min(100, +r.pct || 0));
      // 红色预警由 payload 显式 r.hot 决定(额度行 pct 是「剩余」,低才危险,跟 used 类相反,
      // 不能用 pct>=90 判;ctx 行另走 refreshContext 自己判 used>=90)。
      var cls = 'cqb' + (r.cls === 'local' ? ' local' : '') + (r.hot ? ' hot' : '');
      var wrap = el('div', cls);
      var top = el('div', 'cqt');
      top.appendChild(el('span', 'cql', r.label || ''));
      top.appendChild(el('span', 'cqd', r.detail || ''));
      wrap.appendChild(top);
      var track = el('div', 'cqk');
      var fill = el('i');
      fill.style.width = pct + '%';
      track.appendChild(fill);
      wrap.appendChild(track);
      return wrap;
    }
    var s = el('div', 'cqs');
    s.appendChild(el('span', 'cql', (r && r.label) || ''));
    s.appendChild(el('span', 'cqd', (r && r.detail) || ''));
    return s;
  }

  function renderInto(node, data) {
    node.textContent = '';
    node.classList.toggle('cqcol', isCollapsed());
    node.appendChild(buildHeader(node, data));
    var body = el('div', 'cqbody');
    (data.rows || []).forEach(function(r) { body.appendChild(buildRow(r)); });
    node.appendChild(body);
  }

  function ensureNode() {
    var data = window.__catQuotaLast;
    var node = document.getElementById('cat-quota-entry');
    if (!data || !data.rows || !data.rows.length) { if (node) node.remove(); return; }
    var scroller = findScroller();
    if (!scroller) { if (node) node.remove(); return; }
    ensureStyle();
    var fresh = false;
    if (!node || node.parentElement !== scroller) {
      if (node) node.remove();
      node = document.createElement('section');
      node.id = 'cat-quota-entry';
      scroller.appendChild(node);
      fresh = true;
    } else if (node !== scroller.lastElementChild) {
      // React 后续往 scroller 追加 section 会把条目挤到中间;appendChild
      // 对已存在节点是移动,保持条目恒在弹窗末尾
      scroller.appendChild(node);
    }
    // 内容只在数据变化(或节点新建)时重建,避免 observer 高频 re-render churn
    // (折叠态走 classList 直接切,不触发重建,故折叠不丢)
    var sig = JSON.stringify(data);
    if (fresh || sig !== window.__catQuotaSig) {
      renderInto(node, data);
      window.__catQuotaSig = sig;
    }
    if (data.title) node.title = data.title;
    // 上下文 + 实时速率 + 累计/缓存(对话隔离)每次都刷(observer 触发即更新 → 实时)。
    // cid 一次算、refreshTps/refreshDuo 复用(避免每个各爬一次 fiber)。
    var cid = readConvId();
    refreshContext(node, cid);
    refreshTps(node, cid);
    refreshDuo(node, cid);
  }

  window.__catQuotaUpdate = function(data) {
    window.__catQuotaLast = (data && data.rows && data.rows.length) ? data : null;
    if (!window.__catQuotaLast) window.__catQuotaSig = null;
    ensureNode();
  };

  // rAF 合并:streaming 时 body subtree 高频变更,逐次跑 querySelectorAll 太热。
  // 同一 observer 顺带喂实时 tps(accumulateTps)——流式文本变更正是它要的信号。
  var scheduled = false;
  var mo = new MutationObserver(function(muts) {
    accumulateTps(muts);
    if (!window.__catQuotaLast || scheduled) return;
    scheduled = true;
    requestAnimationFrame(function() { scheduled = false; ensureNode(); });
  });
  mo.observe(document.body, {
    childList: true,
    subtree: true,
    characterData: true,
    characterDataOldValue: true,
  });
  window.__catQuotaObserver = mo;
  // 置位放最后:若上方任一步抛异常(如极早期 document.body 为 null),
  // guard 不毒化,下一 tick 重装(review MEDIUM-2)
  window.__catQuotaInstalled = true;
})();
"##;

/// 卸载脚本:断 observer、删全局态、拆 DOM 节点 + scoped style。幂等。
const REMOVE_SCRIPT: &str = r#"
(function() {
  if (window.__catQuotaObserver) { window.__catQuotaObserver.disconnect(); }
  delete window.__catQuotaObserver;
  delete window.__catQuotaUpdate;
  delete window.__catActiveConvId;
  delete window.__catQuotaLast;
  delete window.__catQuotaSig;
  delete window.__catQuotaVersion;
  delete window.__catQuotaInstalled;
  var n = document.getElementById('cat-quota-entry');
  if (n) n.remove();
  var s = document.getElementById('cat-quota-style');
  if (s) s.remove();
})();
"#;

/// evaluate 失败的阶段 —— 决定日志级别(review HIGH-1):
/// - `Connect`:CDP 端口未就绪 / 连不上 = Codex 没跑,**常态**,debug 级。
/// - `Evaluate`:ws 已建立后 evaluate 被拒 / 注入 JS 抛异常 / 响应超时 =
///   真异常(Codex 在跑但注入坏了),warn 级(首次,去重防 5s 刷屏)。
enum PushError {
    Connect(String),
    Evaluate(String),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::Connect(e) => write!(f, "{e}"),
            PushError::Evaluate(e) => write!(f, "{e}"),
        }
    }
}

/// 一次 CDP 推送:install(幂等)+ `__catQuotaUpdate(payload)`。
/// payload=None 表示"当前无可显示数据"(条目隐藏但保持已安装)。
/// payload 为结构化对象(见 [`INSTALL_SCRIPT`] schema):`{header,title,rows[]}`。
/// 推一次 payload,并**回读当前活动会话 conversationId**(脚本末尾返回
/// `__catActiveConvId()`,MOC-230 对话隔离用)。返回 `Ok(Some(convId))` / `Ok(None)`(读不到)。
async fn push_via_cdp(payload: Option<serde_json::Value>) -> Result<Option<String>, PushError> {
    let update_arg = payload.unwrap_or(serde_json::Value::Null);
    // update 调用拼在 install 后:首次/页面重载后 install 真正执行,平时跳过。末尾表达式
    // 返回活动 conversationId → evaluate 回读(daemon 下 tick 据此按 uuid 取该对话累计)。
    let script = format!(
        "{INSTALL_SCRIPT}\nwindow.__catQuotaUpdate && window.__catQuotaUpdate({update_arg});\n(window.__catActiveConvId ? window.__catActiveConvId() : null);"
    );
    evaluate_once(&script).await
}

/// token 数紧凑格式:850 → `850`、42100 → `42.1k`、1_250_000 → `1.25M`。
fn fmt_tokens(n: u64) -> String {
    // 累计 token 无上限,≥1000M 用 B(十亿)避免「1000.00M」挤爆行。k→M→B。
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// resetTime(RFC3339)→ 本地时间点「MM-DD HH:MM」。解析失败 → None。
/// 用绝对时间点而非剩余时长,避免静态推送下倒计时快速过期。
fn fmt_reset_local(rfc3339: Option<&str>) -> Option<String> {
    let s = rfc3339?;
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    Some(
        dt.with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string(),
    )
}

/// 单条额度 bar(5h / weekly):显**剩余**百分比(满额=100,条满)+ 绝对重置时间点;
/// 剩余 ≤10% 标红预警(`hot`,由 JS 上色——额度类红色判定与 used 类相反,故显式传)。
fn quota_bar(w: &QuotaWindow) -> serde_json::Value {
    let pct = w.remaining_percent.round() as i64;
    let detail = match fmt_reset_local(w.reset_rfc3339.as_deref()) {
        Some(t) => format!("{pct}% · {t} 刷新"),
        None => format!("{pct}%"),
    };
    json!({"kind":"bar","cls":"quota","label":w.label,"pct":pct,"detail":detail,"hot":pct <= 10})
}

/// [MOC-204 / MOC-211] 额度行:白名单 provider 的每个窗口渲染一条 bar(antigravity/GLM =
/// 5h + 每周;MiMo = 套餐用量月窗口)。label 随窗口走,故 5h/周 与 月度套餐各显各、互不混。
/// 非白名单 / 拿不到 → 空(不显额度行)。
fn quota_rows(quota: Option<&ProviderQuota>) -> Vec<serde_json::Value> {
    let Some(q) = quota else {
        return vec![];
    };
    // 先窗口 bar(5h/周/套餐用量),再数值条目(DeepSeek 余额等)。stat 行无进度条:
    // 注入脚本 buildRow 对非 bar/duo/ctx 的 kind 落 `cqs`(label + detail)分支渲染。
    let mut rows: Vec<serde_json::Value> = q.windows.iter().map(quota_bar).collect();
    rows.extend(
        q.stats
            .iter()
            .map(|s| json!({"kind":"stat","label":s.label,"detail":s.value})),
    );
    rows
}

/// [MOC-204 Phase 2] 本地用量两行:
/// - **上下文**:`{"kind":"ctx"}` —— 注入脚本直接读 Codex 自己的上下文 %(composer 旁
///   `[title^="Context usage:"]` 环,常驻 DOM),立即显示、跟 Codex 完全一致、不需对话
///   触发。Codex 常驻 DOM 只暴露 %(绝对 62k/190k 仅 hover tooltip,读不到),故只显 %。
/// - **Tokens 速率·累计 / 缓存命中率**:累计量,Codex 没有、只能 proxy 捕获 → 需对话
///   触发;还没有任何一轮(刚开会话)→ placeholder("—")。
fn local_usage_rows(session_id: Option<&str>) -> Vec<serde_json::Value> {
    // 上下文:bar 的 % / 绝对值由注入脚本直接从 Codex React fiber 读 usedTokens + contextWindow
    // (已有对话即有值、不需发新对话;满窗口 = contextWindow ÷ 0.95 把 5% reserve 加回来)。
    // [MOC-231/232] 附带 by-source 明细:按**活动会话 uuid** 读磁盘持久化的明细。
    // producer 是 adapters::responses::request — 在转换末尾起 spawn_blocking 后台算 o200k
    // 逐 item tokenize + 按 conv uuid 落盘(MOC-232 已搬离转发关键路径,不再经 proxy 写盘)。
    // 对话隔离 + 持久(transfer 重启即用、不需新对话)+ 读取快(小 JSON),
    // 与累计/缓存同口径(session_id == rollout/fiber uuid)。有则塞进 ctx 行,注入脚本在文字行
    // 最右加 caret 展开 Claude 风格下拉;该对话还没发过(无文件)/ passthrough → None,不显 caret。
    let ctx =
        match session_id.and_then(codex_app_transfer_adapters::responses::load_context_breakdown) {
            Some(breakdown) => json!({"kind": "ctx", "label": "上下文", "breakdown": breakdown}),
            None => json!({"kind": "ctx", "label": "上下文"}),
        };
    // 累计 token + 缓存命中率:按**活动会话 uuid** 取该对话自己的 rollout(MOC-230 对话隔离,
    // 非 newest-mtime)。session_id = daemon 上一 tick 从 Codex fiber 回读的 conversationId
    // (== rollout 文件名 uuid);None / 该 uuid 无 rollout 文件(全新对话还没写盘 / 读不到 id)
    // → fail-closed 显「—」,绝不退 newest-mtime 串到别的对话。rollout 含全部历史轮次、compact
    // 已正确计入,不需发新对话。速率(token/s)由注入脚本实时从流式文本估算,payload 不带 rate。
    let totals = session_id.and_then(codex_app_transfer_usage_tracker::session_totals_for_id);
    let cum_part = match totals {
        Some(t) if t.total_tokens > 0 => format!("累计 {}", fmt_tokens(t.total_tokens)),
        _ => "累计 —".to_string(),
    };
    let right = match totals.and_then(|t| t.cache_hit_percent()) {
        Some(p) => format!("缓存命中 {}%", p.round() as i64),
        None => "缓存命中 —".to_string(),
    };
    let duo = json!({"kind": "duo", "cum": cum_part, "right": right});
    vec![ctx, duo]
}

/// 组装完整 Usage 面板 payload:额度(白名单 provider 真实,否则不显)+ 本地实时
/// (上下文/Tokens/缓存)。`quota` 由 daemon 在活动 provider 为白名单时传入。
fn build_payload(quota: Option<&ProviderQuota>, session_id: Option<&str>) -> serde_json::Value {
    let mut rows = quota_rows(quota);
    rows.extend(local_usage_rows(session_id));
    json!({
        "header": "Usage",
        "title": "MOC-204 · 额度(白名单 provider)+ 上下文/Tokens/缓存(本地实时)",
        // MOC-230 对话隔离:标注本累计/缓存是为哪个 conversationId 算的;JS 渲染前比对当前
        // fiber conversationId,不匹配则隐藏累计/缓存(切对话瞬间不串)。
        "convId": session_id,
        "rows": rows,
    })
}

/// [仅测试] JS↔payload 契约固定 fixture(代表性满数据 4 行:2 额度 bar + 上下文 bar
/// + Tokens/缓存 duo),锁结构不随运行期 usage 状态漂移。
#[cfg(test)]
fn build_mock_payload() -> serde_json::Value {
    let mut rows = vec![
        json!({"kind":"bar","cls":"quota","label":"5 小时额度","pct":94,"detail":"94% · 06-13 17:56 刷新","hot":false}),
        json!({"kind":"bar","cls":"quota","label":"每周额度","pct":100,"detail":"100% · 06-20 12:56 刷新","hot":false}),
    ];
    // [MOC-231] ctx 行带 by-source 明细(代表性满数据:tool 调用大头,对齐真机实测占比),
    // 锁 payload↔JS 契约不随运行期漂移。
    rows.push(json!({"kind":"ctx","label":"上下文","breakdown":{
        "total_tokens": 267000,
        "categories": [
            {"key":"tool_calls","tokens":168000,"items":606},
            {"key":"messages","tokens":62000,"items":223},
            {"key":"reasoning","tokens":20000,"items":157},
            {"key":"developer","tokens":10000,"items":3},
            {"key":"tools","tokens":6700,"items":18},
            {"key":"system_prompt","tokens":86,"items":1}
        ]
    }}));
    rows.push(json!({"kind":"duo","cum":"累计 128.5k","right":"缓存命中 67%"}));
    json!({ "header": "Usage", "title": "fixture", "rows": rows })
}

/// 推一次卸载脚本(开关关闭 / 启动清残留时调用)。
async fn push_remove() -> Result<(), PushError> {
    evaluate_once(REMOVE_SCRIPT).await.map(|_| ())
}

/// evaluate 一段脚本,返回其末尾表达式的 JS 字符串结果(非字符串 / null / 无 → None)。
async fn evaluate_once(script: &str) -> Result<Option<String>, PushError> {
    let ws_url = locate_main_window_ws()
        .await
        .map_err(|e| PushError::Connect(e.to_string()))?;
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .map_err(|e| PushError::Connect(e.to_string()))?;
    let (mut write, mut read) = ws_stream.split();
    let (msg, _) = make_msg(
        1,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
    );
    write
        .send(WsMessage::Text(msg.into()))
        .await
        .map_err(|e| PushError::Evaluate(e.to_string()))?;
    let value = drain_until_response(&mut read, 1)
        .await
        .map_err(PushError::Evaluate)?;
    let _ = write.close().await;
    Ok(value.and_then(|v| v.as_str().map(str::to_string)))
}

/// 读 settings 的 `codexQuotaEnabled`(默认 false)。
fn quota_enabled() -> bool {
    crate::admin::registry_io::load()
        .ok()
        .as_ref()
        .and_then(|c| c.get("settings"))
        .and_then(|s| s.get("codexQuotaEnabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// 活动 provider 的 authScheme(额度白名单判定用)。无活动 → providers 第一个。
fn active_authscheme() -> Option<String> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let active_id = cfg.get("activeProvider").and_then(|v| v.as_str());
    let providers = cfg.get("providers")?.as_array()?;
    let p = match active_id {
        Some(id) => providers
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id))?,
        None => providers.first()?,
    };
    p.get("authScheme")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// antigravity 额度白名单(**按 authScheme 判定**):gemini 系经 `retrieveUserQuotaSummary`。
/// 注:GLM Coding 不走这里——它是通用 `bearer` authScheme(无法靠 authScheme 区分),改按
/// baseUrl host 判定,见 [`active_glm_provider`](MOC-211)。
fn is_quota_whitelisted(authscheme: &str) -> bool {
    matches!(
        authscheme,
        "google_oauth_antigravity" | "antigravity_oauth" | "antigravity"
    )
}

/// 从 URL 轻量提取 host(剥 scheme / 端口 / userinfo,不引 url crate)。
fn host_of(url: &str) -> Option<String> {
    let after = url.split("://").nth(1).unwrap_or(url);
    let authority = after.split('/').next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_string())
}

/// 活动 provider 若是 GLM Coding(baseUrl host 含 `bigmodel.cn` / `z.ai` 且为 coding 套餐)→
/// 返回 `(monitor host, apiKey)`,否则 None。**白名单按 baseUrl host 判定而非 authScheme**:
/// GLM 是通用 `bearer`、authScheme 无法区分,且本仓存储的 provider id 是运行时 UUID(非 preset
/// id `zhipu-coding`)。按量计费的 `zhipu`(`/api/paas/v4`,无 coding)不匹配——它无订阅额度窗口。
fn active_glm_provider() -> Option<(String, String)> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let active_id = cfg.get("activeProvider").and_then(|v| v.as_str());
    let providers = cfg.get("providers")?.as_array()?;
    let p = match active_id {
        Some(id) => providers
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id))?,
        None => providers.first()?,
    };
    let base_url = p.get("baseUrl").and_then(|v| v.as_str())?;
    let host = host_of(base_url)?;
    let is_glm_host = host.ends_with("bigmodel.cn") || host.ends_with("z.ai");
    let is_coding = base_url.contains("coding");
    if !(is_glm_host && is_coding) {
        return None;
    }
    let api_key = p
        .get("apiKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    Some((host, api_key.to_string()))
}

/// [MOC-211] 活动 provider 若是 MiMo Token Plan(baseUrl host 含 `xiaomimimo.com` 且路径含
/// `token-plan`)且已存网页 session → 返回 `(provider id, mimoCookie)`,否则 None。按量计费的
/// MiMo(`api.xiaomimimo.com`,无 token-plan)不匹配——它无订阅套餐额度。额度查询走小米账号
/// session(见 mimo_quota / mimo_session),非 apiKey。
fn active_mimo_session() -> Option<(String, String)> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let active_id = cfg.get("activeProvider").and_then(|v| v.as_str());
    let providers = cfg.get("providers")?.as_array()?;
    let p = match active_id {
        Some(id) => providers
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id))?,
        None => providers.first()?,
    };
    let base_url = p.get("baseUrl").and_then(|v| v.as_str())?;
    let host = host_of(base_url)?;
    if !(host.ends_with("xiaomimimo.com") && base_url.contains("token-plan")) {
        return None;
    }
    let id = p.get("id").and_then(|v| v.as_str())?.to_string();
    let cookie = p
        .get("mimoCookie")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    Some((id, cookie))
}

/// session 失效(Auth)时清掉该 provider 存的 `mimoCookie`,让前端 `hasMimoCookie` 转 false
/// 显「未登录」、提示重新登录(MiMo session 无 refresh,只能重登)。
fn clear_mimo_cookie(provider_id: &str) {
    let _ = crate::admin::registry_io::with_config_write(|cfg| {
        let Some(providers) = cfg
            .as_object_mut()
            .and_then(|o| o.get_mut("providers"))
            .and_then(|v| v.as_array_mut())
        else {
            return Ok(crate::admin::registry_io::ConfigMutation::Unchanged(()));
        };
        let mut changed = false;
        for p in providers.iter_mut() {
            if p.get("id").and_then(|v| v.as_str()) == Some(provider_id) {
                if let Some(obj) = p.as_object_mut() {
                    if obj.remove("mimoCookie").is_some() {
                        changed = true;
                    }
                }
                break;
            }
        }
        if changed {
            Ok(crate::admin::registry_io::ConfigMutation::Modified(()))
        } else {
            Ok(crate::admin::registry_io::ConfigMutation::Unchanged(()))
        }
    });
}

/// [MOC-211] 活动 provider 若是 DeepSeek(baseUrl host 含 `deepseek.com`)且有 apiKey →
/// 返回 `(host, apiKey)`,否则 None。DeepSeek 余额走推理同一把 key(Bearer),官方
/// `/user/balance` 接口。
fn active_deepseek_provider() -> Option<(String, String)> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let active_id = cfg.get("activeProvider").and_then(|v| v.as_str());
    let providers = cfg.get("providers")?.as_array()?;
    let p = match active_id {
        Some(id) => providers
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id))?,
        None => providers.first()?,
    };
    let base_url = p.get("baseUrl").and_then(|v| v.as_str())?;
    let host = host_of(base_url)?;
    if !host.ends_with("deepseek.com") {
        return None;
    }
    let api_key = p
        .get("apiKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    Some((host, api_key.to_string()))
}

/// [MOC-211] 活动 provider 若是 anyrouter(baseUrl host 含 `anyrouter.top`)且有 apiKey →
/// 返回 `(host, apiKey)`,否则 None。anyrouter 已用额度走 OpenAI 兼容 billing 端点(推理同
/// 一把 key、Bearer);账户剩余余额被阿里反爬挡、不取(见 anyrouter_quota)。
fn active_anyrouter_provider() -> Option<(String, String)> {
    let cfg = crate::admin::registry_io::load().ok()?;
    let active_id = cfg.get("activeProvider").and_then(|v| v.as_str());
    let providers = cfg.get("providers")?.as_array()?;
    let p = match active_id {
        Some(id) => providers
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id))?,
        None => providers.first()?,
    };
    let base_url = p.get("baseUrl").and_then(|v| v.as_str())?;
    let host = host_of(base_url)?;
    if !host.ends_with("anyrouter.top") {
        return None;
    }
    let api_key = p
        .get("apiKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    Some((host, api_key.to_string()))
}

/// 取 antigravity gemini 双窗口额度(白名单 gate + token 校验 + 45s 缓存)。非白名单 /
/// token 失效 → 清缓存 + None(不显额度行)。token 复用文件 refresh_token 刷新(同 app)。
async fn fetch_antigravity_quota(
    http: &Option<reqwest::Client>,
    cache: &mut Option<(
        codex_app_transfer_gemini_oauth::GeminiQuota,
        std::time::Instant,
    )>,
) -> Option<codex_app_transfer_gemini_oauth::GeminiQuota> {
    use codex_app_transfer_gemini_oauth::{
        ensure_valid_antigravity_token, fetch_gemini_quota_summary, QuotaError, TokenStore,
        ANTIGRAVITY_PROVIDER,
    };
    const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(45);
    // 非白名单(provider 不是 antigravity / 已切走)→ 清缓存,不显额度行(防上个 provider
    // 的额度滞留)。
    if !active_authscheme()
        .as_deref()
        .is_some_and(is_quota_whitelisted)
    {
        *cache = None;
        return None;
    }
    let http = http.as_ref()?;
    let store = TokenStore::for_token_filename(ANTIGRAVITY_PROVIDER.token_filename).ok()?;
    // token 校验**前置于 TTL 命中**:登出 / 刷新失败(NotLoggedIn 等)→ 立即清缓存 + 不显
    // 额度,不让上个账号的额度滞留(review P2:clear cached quota when token disappears;
    // 旧实现把 token 校验放 TTL 之后、且失败返旧缓存 → 登出后旧额度残留 ≤45s 甚至更久)。
    // 校验本地廉价(仅临近过期才走网络 refresh),每 tick 走一遍可接受;失败留 debug 面包屑
    // (非静默,silent-failure LOW-1)。
    let token = match ensure_valid_antigravity_token(http, &store).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(error = %e, "[Quota] antigravity token 不可用(登出/刷新失败)→ 清额度缓存、暂不显额度行");
            *cache = None;
            return None;
        }
    };
    // token 有效:45s 内复用缓存,避免每 5s tick 都打 cloudcode-pa。
    if let Some((q, at)) = cache.as_ref() {
        if at.elapsed() < QUOTA_TTL {
            return Some(q.clone());
        }
    }
    match fetch_gemini_quota_summary(http, &token).await {
        Ok(q) => {
            *cache = Some((q.clone(), std::time::Instant::now()));
            Some(q)
        }
        // 服务端撤销 token(401/403,本地文件还看着有效)→ 清缓存,不残留上个账号/状态的额度
        // (review P2:clear quota cache on auth failures —— 补 token 本地校验通过但服务端已失效
        // 的缺口,跟 ensure_valid_antigravity_token 失败清缓存对称)。
        Err(QuotaError::Auth(s)) => {
            tracing::debug!(status = %s, "[Quota] retrieveUserQuotaSummary 鉴权失败(token 服务端失效)→ 清额度缓存");
            *cache = None;
            None
        }
        // 网络 / 5xx / 429 / 解析瞬时失败:同账号,留旧缓存 + 刷新时间戳(下个 TTL 周期再试,
        // 不每 tick 重打 cloudcode-pa);旧值短时展示可接受。
        Err(QuotaError::Transient(e)) => {
            tracing::debug!(error = %e, "[Quota] quota fetch 瞬时失败,留旧缓存(下个 TTL 周期重试)");
            if let Some((_, at)) = cache.as_mut() {
                *at = std::time::Instant::now();
            }
            cache.as_ref().map(|(q, _)| q.clone())
        }
    }
}

/// [MOC-211] 取 GLM Coding 双窗口额度(baseUrl host gate + 45s 缓存)。非 GLM coding provider /
/// key 失效 → 清缓存 + None(不显额度行)。用推理同一把 apiKey 查 monitor 端口(不带 Bearer),
/// 失败按 [`crate::glm_quota::QuotaError`] 分类(Auth=清缓存 / Transient=留旧缓存重试),
/// 与 antigravity 同一套缓存语义。
async fn fetch_glm_quota(
    http: &Option<reqwest::Client>,
    cache: &mut Option<(ProviderQuota, std::time::Instant)>,
) -> Option<ProviderQuota> {
    use crate::glm_quota::{fetch_glm_quota_summary, QuotaError};
    const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(45);
    // 活动 provider 不是 GLM coding(或已切走)→ 清缓存,不残留上个 provider 的额度。
    let Some((host, api_key)) = active_glm_provider() else {
        *cache = None;
        return None;
    };
    if let Some((q, at)) = cache.as_ref() {
        if at.elapsed() < QUOTA_TTL {
            return Some(q.clone());
        }
    }
    let http = http.as_ref()?;
    match fetch_glm_quota_summary(http, &host, &api_key).await {
        Ok(q) => {
            *cache = Some((q.clone(), std::time::Instant::now()));
            Some(q)
        }
        // 401/403:key 服务端失效 / 非 coding-plan key → 清缓存,不残留。
        Err(QuotaError::Auth(s)) => {
            tracing::debug!(status = %s, "[Quota] GLM quota 鉴权失败(key 失效/非 coding key)→ 清额度缓存");
            *cache = None;
            None
        }
        // 网络 / 5xx / 429 / 解析瞬时失败:留旧缓存 + 刷新时间戳(下个 TTL 周期再试)。
        Err(QuotaError::Transient(e)) => {
            tracing::debug!(error = %e, "[Quota] GLM quota 瞬时失败,留旧缓存(下个 TTL 周期重试)");
            if let Some((_, at)) = cache.as_mut() {
                *at = std::time::Instant::now();
            }
            cache.as_ref().map(|(q, _)| q.clone())
        }
    }
}

/// [MOC-211] 取 MiMo 套餐用量(host gate + 网页 session cookie + 45s 缓存)。非 MiMo token-plan /
/// 无 cookie → 清缓存 + None。session 失效(Auth)→ 清缓存 + 清存储 cookie(前端转「未登录」
/// 提示重登;MiMo session 无 refresh,只能重新登录)。
async fn fetch_mimo_quota(
    http: &Option<reqwest::Client>,
    cache: &mut Option<(ProviderQuota, std::time::Instant)>,
) -> Option<ProviderQuota> {
    use crate::mimo_quota::{fetch_mimo_quota_summary, QuotaError};
    const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(45);
    let Some((id, cookie)) = active_mimo_session() else {
        *cache = None;
        return None;
    };
    if let Some((q, at)) = cache.as_ref() {
        if at.elapsed() < QUOTA_TTL {
            return Some(q.clone());
        }
    }
    let http = http.as_ref()?;
    match fetch_mimo_quota_summary(http, &cookie).await {
        Ok(q) => {
            *cache = Some((q.clone(), std::time::Instant::now()));
            Some(q)
        }
        Err(QuotaError::Auth) => {
            tracing::debug!("[Quota] MiMo session 失效 → 清缓存 + 清存储 cookie(需重新登录)");
            *cache = None;
            clear_mimo_cookie(&id);
            None
        }
        Err(QuotaError::Transient(e)) => {
            tracing::debug!(error = %e, "[Quota] MiMo quota 瞬时失败,留旧缓存(下个 TTL 周期重试)");
            if let Some((_, at)) = cache.as_mut() {
                *at = std::time::Instant::now();
            }
            cache.as_ref().map(|(q, _)| q.clone())
        }
    }
}

/// [MOC-211] 取 DeepSeek 账户余额(host gate + 45s 缓存)。非 DeepSeek / key 失效 → 清缓存 +
/// None。用推理同一把 apiKey 查官方 `/user/balance`。
async fn fetch_deepseek_quota(
    http: &Option<reqwest::Client>,
    cache: &mut Option<(ProviderQuota, std::time::Instant)>,
) -> Option<ProviderQuota> {
    use crate::deepseek_quota::{fetch_deepseek_balance, QuotaError};
    const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(45);
    let Some((host, api_key)) = active_deepseek_provider() else {
        *cache = None;
        return None;
    };
    if let Some((q, at)) = cache.as_ref() {
        if at.elapsed() < QUOTA_TTL {
            return Some(q.clone());
        }
    }
    let http = http.as_ref()?;
    match fetch_deepseek_balance(http, &host, &api_key).await {
        Ok(q) => {
            *cache = Some((q.clone(), std::time::Instant::now()));
            Some(q)
        }
        Err(QuotaError::Auth(s)) => {
            tracing::debug!(status = %s, "[Quota] DeepSeek balance 鉴权失败(key 失效)→ 清缓存");
            *cache = None;
            None
        }
        Err(QuotaError::Transient(e)) => {
            tracing::debug!(error = %e, "[Quota] DeepSeek balance 瞬时失败,留旧缓存(下个 TTL 周期重试)");
            if let Some((_, at)) = cache.as_mut() {
                *at = std::time::Instant::now();
            }
            cache.as_ref().map(|(q, _)| q.clone())
        }
    }
}

/// [MOC-211] 取 anyrouter 已用额度(host gate + 45s 缓存)。非 anyrouter / key 失效 → 清缓存 +
/// None。用推理同一把 apiKey 查 OpenAI 兼容 `/v1/dashboard/billing/usage`(end_date=本地当天)。
async fn fetch_anyrouter_quota(
    http: &Option<reqwest::Client>,
    cache: &mut Option<(ProviderQuota, std::time::Instant)>,
) -> Option<ProviderQuota> {
    use crate::anyrouter_quota::{fetch_anyrouter_usage, QuotaError};
    const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(45);
    let Some((host, api_key)) = active_anyrouter_provider() else {
        *cache = None;
        return None;
    };
    if let Some((q, at)) = cache.as_ref() {
        if at.elapsed() < QUOTA_TTL {
            return Some(q.clone());
        }
    }
    let http = http.as_ref()?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    match fetch_anyrouter_usage(http, &host, &api_key, &today).await {
        Ok(q) => {
            *cache = Some((q.clone(), std::time::Instant::now()));
            Some(q)
        }
        Err(QuotaError::Auth(s)) => {
            tracing::debug!(status = %s, "[Quota] anyrouter usage 鉴权失败(key 失效)→ 清缓存");
            *cache = None;
            None
        }
        Err(QuotaError::Transient(e)) => {
            tracing::debug!(error = %e, "[Quota] anyrouter usage 瞬时失败,留旧缓存(下个 TTL 周期重试)");
            if let Some((_, at)) = cache.as_mut() {
                *at = std::time::Instant::now();
            }
            cache.as_ref().map(|(q, _)| q.clone())
        }
    }
}

/// 按阶段分级记录推送失败:connect 失败 = Codex 没跑(常态,debug);
/// evaluate 失败 = Codex 在跑但注入坏了(真异常,warn 一次后去重降 debug,
/// 防 5s tick 刷屏;成功后复位再坏会再 warn)。
fn log_push_error(e: &PushError, ctx: &str, evaluate_warned: &mut bool) {
    match e {
        PushError::Connect(msg) => {
            tracing::debug!(error = %msg, "[Quota] {ctx} skipped (Codex not reachable)");
        }
        PushError::Evaluate(msg) => {
            if *evaluate_warned {
                tracing::debug!(error = %msg, "[Quota] {ctx} failed (still failing)");
            } else {
                *evaluate_warned = true;
                tracing::warn!(error = %msg, "[Quota] {ctx} failed after CDP connect — 注入异常(后续同类降 debug)");
            }
        }
    }
}

/// 常驻 daemon:每 tick 读 settings + 快照,推送/清除。在 main.rs 启动时
/// spawn 一次。CDP 不可达(Codex 没跑 / 端口未就绪)时静默跳过本 tick ——
/// 这是常态(Codex 未启动)而非错误,不刷日志。
pub async fn run_quota_daemon() {
    const TICK: Duration = Duration::from_secs(5);
    // 待清理标记(review IMPORTANT):初始 true —— transfer 重启后开关可能
    // 已关而上一会话的条目还挂在 Codex 页面里(冻结数据),首个 off tick 推
    // 一次 remove 清残留;remove 失败(如恰逢 CDP 瞬时不可达)保持 true 下
    // tick 重试,成功才复位。开→关边沿同样置 true 走该路径。
    let mut needs_remove = true;
    let mut evaluate_warned = false;
    // 额度查询用的 http client(建一次复用)+ 45s 缓存(避免每 5s tick 打 cloudcode-pa)。
    let quota_http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .ok();
    let mut quota_cache: Option<(
        codex_app_transfer_gemini_oauth::GeminiQuota,
        std::time::Instant,
    )> = None;
    // [MOC-211] GLM Coding / MiMo Token Plan / DeepSeek 额度缓存(各自独立;各源按活动 provider
    // 互斥、self-gate,最多一个返 Some)。
    let mut glm_cache: Option<(ProviderQuota, std::time::Instant)> = None;
    let mut mimo_cache: Option<(ProviderQuota, std::time::Instant)> = None;
    let mut deepseek_cache: Option<(ProviderQuota, std::time::Instant)> = None;
    let mut anyrouter_cache: Option<(ProviderQuota, std::time::Instant)> = None;
    // MOC-230 对话隔离:上一 tick 从 fiber 回读的活动 conversationId。本 tick 据此按 uuid 取
    // 该对话累计/缓存(非 newest-mtime)。1-tick 延迟由 JS 侧 uuid-guard 兜底(渲染前比对当前
    // fiber id,不匹配则隐藏 → 切对话瞬间不串)。None = 无可识别活动会话 → fail-closed 显「—」。
    let mut last_conv_id: Option<String> = None;
    loop {
        tokio::time::sleep(TICK).await;
        let enabled = quota_enabled();
        // [MOC-231 perf] 跟随面板开关同步 adapter 侧门禁:关闭时 proxy 转发跳过 o200k 逐 item
        // tokenize,不在热路径白算 breakdown(默认关 → 绝大多数请求免算)。
        codex_app_transfer_adapters::responses::set_breakdown_enabled(enabled);
        if !enabled {
            if needs_remove {
                match push_remove().await {
                    Ok(()) => needs_remove = false,
                    Err(e) => log_push_error(&e, "remove push", &mut evaluate_warned),
                }
            }
            continue;
        }
        needs_remove = true;
        // 额度:白名单 provider 取真实双窗口(各自归一成 ProviderQuota),否则 None(不显额度行);
        // 上下文/Tokens/缓存命中率为本地实时(注入脚本侧)。两个 fetcher 各按活动 provider self-gate,
        // antigravity(authScheme)与 GLM(baseUrl host)互斥 → 最多一个返 Some,or() 取活动那个;
        // 非活动源每 tick 自清缓存(防上个 provider 额度滞留)。
        let antigravity = fetch_antigravity_quota(&quota_http, &mut quota_cache)
            .await
            .map(ProviderQuota::from);
        let glm = fetch_glm_quota(&quota_http, &mut glm_cache).await;
        let mimo = fetch_mimo_quota(&quota_http, &mut mimo_cache).await;
        let deepseek = fetch_deepseek_quota(&quota_http, &mut deepseek_cache).await;
        let anyrouter = fetch_anyrouter_quota(&quota_http, &mut anyrouter_cache).await;
        // 取活动那个源(互斥,最多一个 Some);空额度(无窗口无条目)→ 视作无,不显额度行。
        let quota = antigravity
            .or(glm)
            .or(mimo)
            .or(deepseek)
            .or(anyrouter)
            .filter(ProviderQuota::has_any);
        // 累计/缓存按上 tick 回读的活动 conversationId 取(MOC-230);payload 标注该 id。
        let payload = Some(build_payload(quota.as_ref(), last_conv_id.as_deref()));
        match push_via_cdp(payload).await {
            // push 同时回读**当前**活动 conversationId,供下 tick 按 uuid 取累计。
            // Ok(None)=evaluate 成功但无可识别活动会话 → 下 tick fail-closed 显「—」。
            Ok(conv) => {
                evaluate_warned = false;
                last_conv_id = conv;
            }
            Err(e) => log_push_error(&e, "quota push", &mut evaluate_warned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_is_idempotent_and_remove_cleans() {
        // 结构性断言:防手滑改掉幂等 guard / 清理目标
        // [MOC-230] 版本化幂等 guard:同版本跳过、版本变重装(覆盖旧注入,无需重启 Codex)
        assert!(INSTALL_SCRIPT.contains("window.__catQuotaInstalled"));
        assert!(INSTALL_SCRIPT.contains("__catQuotaVersion === VERSION"));
        assert!(REMOVE_SCRIPT.contains("__catQuotaVersion"));
        assert!(INSTALL_SCRIPT.contains("cat-quota-entry"));
        assert!(REMOVE_SCRIPT.contains("cat-quota-entry"));
        assert!(REMOVE_SCRIPT.contains("disconnect"));
        assert!(REMOVE_SCRIPT.contains("cat-quota-style"));
    }

    #[test]
    fn install_script_collapsible_and_theme_accent() {
        // [MOC-204] ① 可折叠标题:chevron + 折叠态记忆 + body 收起 + hover 才出 chevron/深色底
        assert!(INSTALL_SCRIPT.contains("cqchev"));
        assert!(INSTALL_SCRIPT.contains("catQuotaCollapsed"));
        assert!(INSTALL_SCRIPT.contains("cqcol .cqbody{display:none}"));
        assert!(INSTALL_SCRIPT.contains(".cqbtn:hover .cqchev{opacity:1}")); // 箭头仅 hover button 组
                                                                             // ①补 标题栏常驻深色带用原生 bg token(非 hover 变色;CDP 实测对齐 Sources)
        assert!(INSTALL_SCRIPT.contains(".cqhdr{display:flex;align-items:center;height:28px"));
        assert!(INSTALL_SCRIPT.contains("background:var(--color-token-dropdown-background"));
        assert!(INSTALL_SCRIPT.contains("color:var(--color-token-text-tertiary")); // 标题色对齐原生
        assert!(!INSTALL_SCRIPT.contains(":hover{background")); // 不得再有 hover 变背景
                                                                // ③ 进度条接主题注入的 accent(跟随壁纸);无主题回退中性蓝(非暖色)
        assert!(INSTALL_SCRIPT.contains("var(--cl-accent,#6c83c4)"));
        assert!(INSTALL_SCRIPT.contains("var(--cl-accent-soft,#9aa9d8)"));
        // ② 行标签常规字重(不加粗)
        assert!(INSTALL_SCRIPT.contains(".cql{font-size:13.5px;font-weight:400"));
    }

    #[test]
    fn mock_payload_matches_js_contract() {
        // [MOC-204 Phase 1] 锁 payload↔JS 契约:JS 按 rows[].kind 渲染 bar/duo,
        // bar 读 pct/cls/label/detail,duo 读 left/right;改任一侧不同步即挂这条。
        let p = build_mock_payload();
        assert_eq!(p["header"], "Usage");
        let rows = p["rows"].as_array().expect("rows array");
        assert_eq!(rows.len(), 4, "5h + 周 + 上下文(ctx) + Tokens/缓存合并行");
        // 2 额度 bar + 上下文 ctx(JS 读 Codex 实时)+ Tokens/缓存 duo
        assert_eq!(rows[0]["kind"], "bar");
        assert_eq!(rows[0]["cls"], "quota");
        assert!(rows[0]["pct"].is_number());
        assert_eq!(rows[2]["kind"], "ctx"); // 上下文 = 注入脚本直接读 Codex
        assert_eq!(rows[3]["kind"], "duo");
        assert!(rows[3]["cum"].is_string() && rows[3]["right"].is_string());
        // 速率(token/s)实时由注入脚本从流式文本估算,payload 不带 rate;JS 端逻辑须在场
        assert!(INSTALL_SCRIPT.contains("accumulateTps") && INSTALL_SCRIPT.contains("currentTps"));
        assert!(INSTALL_SCRIPT.contains("cqrate"));
        assert!(INSTALL_SCRIPT.contains("token/s")); // 单位不缩写成 tok/s
                                                     // ④ 额度重置用绝对时间点(含「刷新」)而非剩余时长
        let d5h = rows[0]["detail"].as_str().unwrap();
        assert!(d5h.contains("刷新") && d5h.contains(':'));
        // 两条额度 bar 行有 label/detail
        for r in &rows[0..2] {
            assert!(r["label"].is_string() && r["detail"].is_string());
        }
        // JS 端消费这些 kind 的 render 分支必须在场
        assert!(INSTALL_SCRIPT.contains("r.kind === 'bar'"));
        assert!(INSTALL_SCRIPT.contains("r.kind === 'duo'"));
        assert!(INSTALL_SCRIPT.contains("r.kind === 'ctx'"));
        assert!(INSTALL_SCRIPT.contains("data.rows"));
        // ctx 直接读 Codex 自己的上下文:优先 React fiber 的 contextUsage(已有对话即有值、
        // 不需新对话),退回 aria-label「Context usage: N%」
        assert!(INSTALL_SCRIPT.contains("Context usage:"));
        assert!(INSTALL_SCRIPT.contains("aria-label^="));
        assert!(INSTALL_SCRIPT.contains("refreshContext"));
        assert!(INSTALL_SCRIPT.contains("__reactFiber$"));
        assert!(INSTALL_SCRIPT.contains("usedTokens") && INSTALL_SCRIPT.contains("contextWindow"));
        assert!(INSTALL_SCRIPT.contains("/ 0.95")); // 满窗口 = 有效窗口 ÷ 0.95(加回 5%)
        assert!(INSTALL_SCRIPT.contains("fmtTok")); // 1M 模型显 1M 而非 1000k
    }

    #[test]
    fn install_script_has_conversation_isolation_guards() {
        // [MOC-230] 对话隔离:fiber 读 conversationId + 暴露给 daemon 回读;tps 按会话重置;
        // 累计/缓存渲染前 uuid-match 守卫(切对话不串)。
        assert!(INSTALL_SCRIPT.contains("function readConvId"));
        assert!(INSTALL_SCRIPT.contains("window.__catActiveConvId")); // daemon 经 evaluate 回读
        assert!(INSTALL_SCRIPT.contains("__tpsConvId")); // tps 按会话隔离(切对话清零)
        assert!(INSTALL_SCRIPT.contains("function refreshDuo")); // 累计/缓存 uuid-match 守卫
        assert!(INSTALL_SCRIPT.contains("cqcum") && INSTALL_SCRIPT.contains("cqcache"));
        assert!(REMOVE_SCRIPT.contains("__catActiveConvId")); // 卸载清理
    }

    #[test]
    fn build_payload_tags_conversation_id() {
        // [MOC-230] payload 标注 convId(JS 渲染前比对当前 fiber conversationId);
        // 无活动会话 → null,JS fail-closed 显「—」不串别的对话。
        let p = build_payload(None, Some("019ec12f-eef0-7971-9bc8-ee9f0c21b5df"));
        assert_eq!(p["convId"], "019ec12f-eef0-7971-9bc8-ee9f0c21b5df");
        let p2 = build_payload(None, None);
        assert!(p2["convId"].is_null());
    }

    #[test]
    fn ctx_breakdown_contract() {
        // [MOC-231] 锁 ctx 行 by-source 明细 payload↔JS 契约。
        let p = build_mock_payload();
        let rows = p["rows"].as_array().expect("rows");
        let ctx = &rows[2];
        assert_eq!(ctx["kind"], "ctx");
        let cats = ctx["breakdown"]["categories"]
            .as_array()
            .expect("ctx.breakdown.categories array");
        assert!(!cats.is_empty());
        assert!(ctx["breakdown"]["total_tokens"].is_number());
        // 每类有 key/tokens(JS 按 key 取 label、按 tokens 算 %)
        for c in cats {
            assert!(c["key"].is_string() && c["tokens"].is_number());
        }
        // JS 端明细渲染分支 + caret + 开合记忆必须在场(改任一侧不同步即挂)
        assert!(INSTALL_SCRIPT.contains("r.breakdown"));
        assert!(INSTALL_SCRIPT.contains("buildBreakdown"));
        assert!(INSTALL_SCRIPT.contains("cqbdcaret"));
        assert!(INSTALL_SCRIPT.contains(".cqb.cqbdopen .cqbd{display:flex}"));
        assert!(INSTALL_SCRIPT.contains("catCtxBdOpen")); // 开合态 localStorage 记忆
                                                          // key→label 映射覆盖后端所有分类 key(对齐 adapters context_breakdown::keys)
        for key in [
            "tool_calls",
            "messages",
            "reasoning",
            "developer",
            "tools",
            "system_prompt",
        ] {
            assert!(
                INSTALL_SCRIPT.contains(&format!("{key}:")),
                "BD_LABELS 缺 key: {key}"
            );
        }
        // [MOC-231] per-conv 用量缓存(localStorage):上下文绝对值 + 累计/缓存「重启即显」。
        // refreshContext 缓存 fiber 绝对值、refreshDuo 缓存累计/缓存命中,(re)load 即显缓存值。
        assert!(INSTALL_SCRIPT.contains("catUsageCache")); // 按 cid 的 localStorage key
        assert!(
            INSTALL_SCRIPT.contains("usageCacheMerge") && INSTALL_SCRIPT.contains("usageCacheGet")
        );
        assert!(INSTALL_SCRIPT.contains("ctxUsed")); // 缓存上下文绝对值(used/window)
                                                     // [MOC-231] breakdown 的 convId 守卫(切对话不串)+ 版本化 guard 已 bump(升级免重启覆盖)
        assert!(INSTALL_SCRIPT.contains("cqbdmismatch"));
        assert!(INSTALL_SCRIPT.contains("var VERSION = 3"));
    }

    #[test]
    fn fmt_tokens_compact() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(850), "850");
        assert_eq!(fmt_tokens(42_100), "42.1k");
        assert_eq!(fmt_tokens(1_250_000), "1.25M");
        // ≥1000M 进 B(十亿),不再「1000.00M」挤爆行
        assert_eq!(fmt_tokens(999_000_000), "999.00M");
        assert_eq!(fmt_tokens(1_000_000_000), "1.00B");
        assert_eq!(fmt_tokens(2_890_000_000), "2.89B");
    }

    #[test]
    fn build_payload_no_quota_hides_quota_rows() {
        // [MOC-204 Phase 3] 非白名单 / 无额度(quota=None)→ 不显额度行,只剩本地 2 行。
        let p = build_payload(None, None);
        let rows = p["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 2, "无额度 → 只有 上下文 + Tokens/缓存");
        assert_eq!(rows[0]["kind"], "ctx");
        assert_eq!(rows[1]["kind"], "duo");
    }

    #[test]
    fn build_payload_with_quota_shows_two_quota_bars() {
        // [MOC-204 / MOC-211] 白名单 provider:5h + weekly 两 bar 在前,显**剩余**百分比。
        // 用中性 ProviderQuota 多窗口(antigravity / GLM 都归一到它)。
        let q = ProviderQuota {
            windows: vec![
                QuotaWindow {
                    label: "5 小时额度".into(),
                    remaining_percent: 94.0, // 剩 94%
                    reset_rfc3339: Some("2026-06-13T17:56:06Z".into()),
                },
                QuotaWindow {
                    label: "每周额度".into(),
                    remaining_percent: 8.0, // 剩 8% → 应标红 hot
                    reset_rfc3339: Some("2026-06-20T12:56:06Z".into()),
                },
            ],
            ..Default::default()
        };
        let p = build_payload(Some(&q), None);
        let rows = p["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 4, "2 额度 + 上下文 + Tokens/缓存");
        assert_eq!(rows[0]["cls"], "quota");
        assert_eq!(rows[0]["label"], "5 小时额度");
        assert_eq!(rows[0]["pct"], 94, "显剩余 94%(满额=100)");
        assert_eq!(rows[0]["hot"], false, "剩余充足不标红");
        assert!(rows[0]["detail"].as_str().unwrap().contains("刷新"));
        assert_eq!(rows[1]["label"], "每周额度");
        assert_eq!(rows[1]["pct"], 8);
        assert_eq!(rows[1]["hot"], true, "剩余 ≤10% 标红预警");
        assert_eq!(rows[2]["kind"], "ctx");
        assert_eq!(rows[3]["kind"], "duo");
    }

    #[test]
    fn quota_whitelist_only_antigravity() {
        assert!(is_quota_whitelisted("google_oauth_antigravity"));
        assert!(is_quota_whitelisted("antigravity_oauth"));
        assert!(!is_quota_whitelisted("zhipu-coding"));
        assert!(!is_quota_whitelisted("google_api_key"));
        assert!(!is_quota_whitelisted("openai_chat"));
    }

    #[test]
    fn quota_rows_empty_when_none() {
        assert!(quota_rows(None).is_empty());
    }

    #[test]
    fn quota_rows_renders_stat_for_balance() {
        // [MOC-211] DeepSeek 余额 = stat 条目(无进度条),JS buildRow 落 cqs(label+detail)。
        use crate::provider_quota::QuotaStat;
        let q = ProviderQuota {
            stats: vec![QuotaStat {
                label: "余额".into(),
                value: "¥5.37".into(),
            }],
            ..Default::default()
        };
        let rows = quota_rows(Some(&q));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "stat");
        assert_eq!(rows[0]["label"], "余额");
        assert_eq!(rows[0]["detail"], "¥5.37");
    }

    #[test]
    fn host_of_extracts_host() {
        // [MOC-211] GLM 白名单 gate 靠它从 baseUrl 取 host。
        assert_eq!(
            host_of("https://open.bigmodel.cn/api/coding/paas/v4").as_deref(),
            Some("open.bigmodel.cn")
        );
        assert_eq!(
            host_of("https://api.z.ai/api/coding/paas/v4").as_deref(),
            Some("api.z.ai")
        );
        // 带端口 / userinfo / 无 scheme 也能剥到纯 host
        assert_eq!(host_of("http://h:8080/x").as_deref(), Some("h"));
        assert_eq!(host_of("https://u@host.tld/x").as_deref(), Some("host.tld"));
        assert_eq!(host_of("open.bigmodel.cn/api").as_deref(), Some("open.bigmodel.cn"));
        assert_eq!(host_of(""), None);
    }
}
