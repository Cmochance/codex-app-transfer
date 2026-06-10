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
  if (window.__catQuotaInstalled) return;
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
      '#cat-quota-entry .cqs{display:flex;align-items:center;justify-content:space-between;gap:10px;padding:7px 16px}';
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
  // 刷新上下文行(每次 ensureNode,含 observer 触发 → live):优先 fiber 精确值(立即、
  // 不需对话);读不到退回 Codex aria 的 %(只有 %)。
  function refreshContext(node) {
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
    } else {
      var aria = readCtxPct();
      pct = aria == null ? 0 : aria;
      detail = aria == null ? '—' : Math.round(aria) + '%';
    }
    // 值相等才不写:textContent/style 赋值会产生 DOM 变更 → 触发本 observer → 若每次都
    // 写就自循环 ~60fps(空闲也跑,code-review IMPORTANT-1)。仅变化时写,稳定即静默。
    var wpx = pct + '%';
    if (d && d.textContent !== detail) d.textContent = detail;
    if (fill && fill.style.width !== wpx) fill.style.width = wpx;
    var hot = pct >= 90;
    if (w.classList.contains('hot') !== hot) w.classList.toggle('hot', hot);
  }

  // ── 实时 tokens/s(MOC-204 §3,参考 Codex 老版 + OpenCode 插件)──
  // SSE 无逐 delta token 数,实时只能按 Codex 流式文本增长估(中文≈0.6 tok/字、其余
  // ≈1/4);2s 滑窗算速率;流停(最近样本 >1.5s)冻结在最后值(用户:保留);没数据 0。
  var __tpsBuf = [];
  var __tpsLast = 0;
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
  function refreshTps(node) {
    var s = node && node.querySelector('.cqrate');
    if (!s) return;
    // 同 refreshContext:仅变化时写,避免自触发 observer 的 ~60fps 空转(流式时值在变 →
    // 正常实时刷;空闲时 currentTps 返回冻结值不变 → 不写 → observer 不被自身唤醒)。
    var t = currentTps() + ' token/s';
    if (s.textContent !== t) s.textContent = t;
  }

  function buildRow(r) {
    if (r && r.kind === 'duo') {
      // 左:实时速率(.cqrate,JS 刷)· 累计(payload);右:缓存命中(payload)
      var d = el('div', 'cqduo');
      var lw = el('span', 'l');
      lw.appendChild(el('span', 'cqrate', '0 token/s'));
      lw.appendChild(document.createTextNode(' · '));
      lw.appendChild(el('span', null, r.cum || '累计 —'));
      d.appendChild(lw);
      d.appendChild(el('span', 'r', r.right || ''));
      return d;
    }
    if (r && r.kind === 'ctx') {
      // 上下文 bar:结构同 local bar,值由 refreshContext 从 Codex 实时填(不靠 payload)
      var cw = el('div', 'cqb local');
      cw.setAttribute('data-ctx', '1'); // 值由 refreshContext 从 Codex fiber 实时填
      var ctop = el('div', 'cqt');
      ctop.appendChild(el('span', 'cql', r.label || '上下文'));
      ctop.appendChild(el('span', 'cqd cqctxd', '—'));
      cw.appendChild(ctop);
      var ctrack = el('div', 'cqk');
      var cfill = el('i');
      cfill.className = 'cqctxfill';
      ctrack.appendChild(cfill);
      cw.appendChild(ctrack);
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
    // 上下文 + 实时速率每次都刷(payload sig 不变也刷,observer 触发即更新 → 实时)
    refreshContext(node);
    refreshTps(node);
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
  delete window.__catQuotaLast;
  delete window.__catQuotaSig;
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
async fn push_via_cdp(payload: Option<serde_json::Value>) -> Result<(), PushError> {
    let update_arg = payload.unwrap_or(serde_json::Value::Null);
    // update 调用拼在 install 后:首次/页面重载后 install 真正执行,平时跳过
    let script = format!(
        "{INSTALL_SCRIPT}\nwindow.__catQuotaUpdate && window.__catQuotaUpdate({update_arg});"
    );
    evaluate_once(&script).await
}

/// token 数紧凑格式:850 → `850`、42100 → `42.1k`、1_250_000 → `1.25M`。
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
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
fn quota_bar(label: &str, w: &codex_app_transfer_gemini_oauth::QuotaWindow) -> serde_json::Value {
    let pct = w.remaining_percent.round() as i64;
    let detail = match fmt_reset_local(w.reset_rfc3339.as_deref()) {
        Some(t) => format!("{pct}% · {t} 刷新"),
        None => format!("{pct}%"),
    };
    json!({"kind":"bar","cls":"quota","label":label,"pct":pct,"detail":detail,"hot":pct <= 10})
}

/// [MOC-204 Phase 3] 额度两行:仅白名单 provider(当前 antigravity gemini 系)有真实额度
/// → 5h + weekly 两 bar;非白名单 / 拿不到 → 空(不显额度行)。
fn quota_rows(
    quota: Option<&codex_app_transfer_gemini_oauth::GeminiQuota>,
) -> Vec<serde_json::Value> {
    let Some(q) = quota else {
        return vec![];
    };
    let mut rows = Vec::new();
    if let Some(w) = &q.five_hour {
        rows.push(quota_bar("5 小时额度", w));
    }
    if let Some(w) = &q.weekly {
        rows.push(quota_bar("每周额度", w));
    }
    rows
}

/// [MOC-204 Phase 2] 本地用量两行:
/// - **上下文**:`{"kind":"ctx"}` —— 注入脚本直接读 Codex 自己的上下文 %(composer 旁
///   `[title^="Context usage:"]` 环,常驻 DOM),立即显示、跟 Codex 完全一致、不需对话
///   触发。Codex 常驻 DOM 只暴露 %(绝对 62k/190k 仅 hover tooltip,读不到),故只显 %。
/// - **Tokens 速率·累计 / 缓存命中率**:累计量,Codex 没有、只能 proxy 捕获 → 需对话
///   触发;还没有任何一轮(刚开会话)→ placeholder("—")。
fn local_usage_rows() -> Vec<serde_json::Value> {
    // 上下文:整行由注入脚本直接从 Codex React fiber 读 usedTokens + contextWindow(已有
    // 对话即有值、不需发新对话;满窗口 = contextWindow ÷ 0.95 把 5% reserve 加回来)。
    // 故 payload 只给占位,不带数据。
    let ctx = json!({"kind": "ctx", "label": "上下文"});
    // 累计 token + 缓存命中率:读 Codex rollout 当前会话(ground truth、含全部历史轮次、
    // compact 已正确计入,**不需发新对话**)。速率(token/s):注入脚本实时从 Codex 流式
    // 文本增长估算(见 INSTALL_SCRIPT 的 tps —— SSE 无逐 delta token,实时只能估),
    // payload 不带 rate。
    let totals = codex_app_transfer_usage_tracker::newest_session_totals();
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
fn build_payload(
    quota: Option<&codex_app_transfer_gemini_oauth::GeminiQuota>,
) -> serde_json::Value {
    let mut rows = quota_rows(quota);
    rows.extend(local_usage_rows());
    json!({
        "header": "Usage",
        "title": "MOC-204 · 额度(白名单 provider)+ 上下文/Tokens/缓存(本地实时)",
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
    rows.push(json!({"kind":"ctx","label":"上下文"}));
    rows.push(json!({"kind":"duo","cum":"累计 128.5k","right":"缓存命中 67%"}));
    json!({ "header": "Usage", "title": "fixture", "rows": rows })
}

/// 推一次卸载脚本(开关关闭 / 启动清残留时调用)。
async fn push_remove() -> Result<(), PushError> {
    evaluate_once(REMOVE_SCRIPT).await
}

async fn evaluate_once(script: &str) -> Result<(), PushError> {
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
    drain_until_response(&mut read, 1)
        .await
        .map_err(PushError::Evaluate)?;
    let _ = write.close().await;
    Ok(())
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

/// 额度白名单 = 能读真实额度的 provider。当前仅 antigravity(gemini 系,经
/// `retrieveUserQuotaSummary`);其余 provider 不显额度行(见 MOC-204 §quota 调研:
/// glm-coding 等无额度 API/头)。
fn is_quota_whitelisted(authscheme: &str) -> bool {
    matches!(
        authscheme,
        "google_oauth_antigravity" | "antigravity_oauth" | "antigravity"
    )
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
    loop {
        tokio::time::sleep(TICK).await;
        let enabled = quota_enabled();
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
        // 额度:仅白名单 provider(antigravity gemini)取真实双窗口,否则 None(不显额度行);
        // 上下文/Tokens/缓存命中率为本地实时(注入脚本侧)。
        let quota = fetch_antigravity_quota(&quota_http, &mut quota_cache).await;
        let payload = Some(build_payload(quota.as_ref()));
        match push_via_cdp(payload).await {
            Ok(()) => evaluate_warned = false,
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
        assert!(INSTALL_SCRIPT.contains("if (window.__catQuotaInstalled) return;"));
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
    fn fmt_tokens_compact() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(850), "850");
        assert_eq!(fmt_tokens(42_100), "42.1k");
        assert_eq!(fmt_tokens(1_250_000), "1.25M");
    }

    #[test]
    fn build_payload_no_quota_hides_quota_rows() {
        // [MOC-204 Phase 3] 非白名单 / 无额度(quota=None)→ 不显额度行,只剩本地 2 行。
        let p = build_payload(None);
        let rows = p["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 2, "无额度 → 只有 上下文 + Tokens/缓存");
        assert_eq!(rows[0]["kind"], "ctx");
        assert_eq!(rows[1]["kind"], "duo");
    }

    #[test]
    fn build_payload_with_quota_shows_two_quota_bars() {
        // [MOC-204 Phase 3] 白名单 provider:5h + weekly 两 bar 在前,显**剩余**百分比。
        use codex_app_transfer_gemini_oauth::{GeminiQuota, QuotaWindow};
        let q = GeminiQuota {
            five_hour: Some(QuotaWindow {
                remaining_percent: 94.0, // 剩 94%
                reset_rfc3339: Some("2026-06-13T17:56:06Z".into()),
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 8.0, // 剩 8% → 应标红 hot
                reset_rfc3339: Some("2026-06-20T12:56:06Z".into()),
            }),
        };
        let p = build_payload(Some(&q));
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
}
