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
//! 存储 schema:`localStorage['catStash'] = [{ id, text, ts, images }]`(全局,不按会话隔离;
//! `images` 为 `[{ src(dataURL), filename }]`,可序列化)。
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
  var VERSION = 9; // review r6:写校验保留前导/尾随空行 + push 遇文件附件中止(保草稿不孤儿化)
  if (window.__catStashInstalled) {
    if (window.__catStashVersion === VERSION) { try { window.__catStashEnsure && window.__catStashEnsure(); } catch (e) {} return; }
    try { if (window.__catStashObserver) window.__catStashObserver.disconnect(); } catch (e) {}
    ['cat-stash-entry', 'cat-stash-bar', 'cat-stash-menu', 'cat-stash-style'].forEach(function(id){ var n = document.getElementById(id); if (n) n.remove(); });
  }
  window.__catStashVersion = VERSION;
  window.__catStashSig = null; // 面板 body 重建签名(见 ensurePanel),重装时清空强制首刷

  // ── 存储(全局 localStorage,跨 reload 存活)──
  var KEY = 'catStash';
  // 解析缓存:load() 会 JSON.parse 整个 blob(含图片 dataURL,可达数 MB)。热路径(ensureBar /
  // ensurePanel 每 tick + 每次打字经 observer→rAF 调 ensure)若每次都重解析就会在敲键时反复
  // parse 数 MB → 卡顿。用一个会话内单调写计数 __rev 做缓存:仅在 save() 后失效一次,其余调用
  // 直接返回上次解析结果(纯内存,连 localStorage 都不读)。单渲染进程、所有写都走 save(),故
  // 缓存与磁盘一致;reload 后 __rev 归 0、首个 load() 重新解析一次。
  // **不可变契约**:load() 返回的数组视为只读,mutate 它的 caller(stashCurrent)须先 .slice()。
  var __rev = 0, __loadRev = -1, __loadVal = [];
  // load 时逐项规整:id/text 强制 string、ts 兜底,屏蔽 schema 漂移或外部写入的非字符串值
  // (否则 renderPanel 的 it.text.replace 会 throw 进而整个面板/按钮静默消失)。
  function load() {
    if (__loadRev === __rev) return __loadVal; // 缓存命中:无写发生,直接复用上次解析结果
    var out = [];
    try {
      var s = localStorage.getItem(KEY); var a = s ? JSON.parse(s) : [];
      if (Array.isArray(a)) {
        out = a.filter(function (x) { return x && x.id != null; }).map(function (x) {
          return {
            id: String(x.id),
            text: x.text == null ? '' : String(x.text),
            ts: x.ts || 0,
            // 图片附件:{src:dataURL, filename}(可序列化;只留带 data: 前缀的)。文件附件注入侧无法
            // 干净恢复(setComposerStateField 不可达 / 合成 drop 无 fs path),不纳入,见 followup CAT-260。
            images: Array.isArray(x.images) ? x.images
              .filter(function (im) { return im && typeof im.src === 'string' && im.src.indexOf('data:') === 0; })
              .map(function (im) { return { src: im.src, filename: String(im.filename || 'image.png') }; }) : []
          };
        });
      }
    } catch (e) { out = []; }
    __loadVal = out; __loadRev = __rev;
    return out;
  }
  // save 返回是否成功:图片 dataURL 较大,localStorage 配额(~5-10MB)可能溢出,caller 据此降级
  // (绝不在保存失败时清空 composer 丢内容)。写成功才 bump __rev 让缓存失效(失败则缓存仍有效)。
  function save(a) { try { localStorage.setItem(KEY, JSON.stringify(a)); __rev++; return true; } catch (e) { return false; } }
  // id 用「ms + 会话内单调序号 + 随机」:序号保证同毫秒/同 random 也不碰撞(碰撞会让
  // filter(x=>x.id!==id) 误删两条)。
  var __seq = 0;
  function uid() { return Date.now().toString(36) + '-' + (++__seq).toString(36) + Math.random().toString(36).slice(2, 6); }
  function findItem(arr, id) { for (var i = 0; i < arr.length; i++) { if (arr[i].id === id) return arr[i]; } return null; }

  // ── composer 操作(.ProseMirror,选择子均 CDP 实证)──
  function composerEl() { return document.querySelector('.ProseMirror'); }
  // 取草稿全文(保留换行):ProseMirror 多行草稿是单 textblock 内的 hard break(<br>)/多 block,
  // textContent 把文本节点直接拼接、丢掉所有换行(`a<br>b` → "ab")。用 innerText 还原渲染所见
  // 的换行(块边界 / <br> → \n),不可用时回退 textContent。**捕获/恢复/校验三处都走它**保持同一
  // 文本空间;空值判断用更廉价的 composerTextRaw(见 ensureBar 热路径)。
  function composerText() { var e = composerEl(); if (!e) return ''; var v = e.innerText; return (v != null ? v : (e.textContent || '')); }
  // 仅判空(textContent 即可,无需 innerText 触发 reflow):热路径用,避免每 tick reflow。
  function composerTextRaw() { var e = composerEl(); return e ? (e.textContent || '') : ''; }
  // 暂存用的规范文本:顶层块(`<p>` 等)即「行」,各块 textContent 为该行文本(本 composer 无
  // hard break,Shift+Enter = 新块);按**单** `\n` join。空块 = 空行,故**保留用户的有意空行**。
  // 这恰是 paste 期望的「每换行一个 `\n`」规范 → restore paste 重建同构块,往返幂等(CDP 实证:
  // `a\n\nb` 空行往返不丢)。**不可用 innerText**(段间 `\n\n`、空段落 `\n` 数非线性,且 paste 回去
  // 会翻倍膨胀)、**不可用 textContent**(丢全部换行 → 多行拼成一行)。
  // 单块取文本:块内**非 trailing** `<br>` → `\n`(防御富文本粘贴/未来版本在块内产生 hard break;
  // CDP 实证本 composer 把 `<br>`/HTML 断行规整成独立段落,正常不触发此分支);否则直接 textContent
  // (空块 = 空行,textContent 为 ''→ blank line 保留;用 innerText 会让空块变 '\n' 破坏空行)。
  function blockText(b) {
    if (b.querySelector && b.querySelector('br:not(.ProseMirror-trailingBreak)')) {
      var c = b.cloneNode(true), brs = c.querySelectorAll('br:not(.ProseMirror-trailingBreak)');
      for (var j = 0; j < brs.length; j++) { brs[j].parentNode.replaceChild(document.createTextNode('\n'), brs[j]); }
      return c.textContent || '';
    }
    return b.textContent || '';
  }
  function captureText() {
    var e = composerEl(); if (!e) return '';
    var ch = e.children;
    if (!ch || !ch.length) return (e.innerText != null ? e.innerText : (e.textContent || ''));
    var lines = [];
    for (var i = 0; i < ch.length; i++) lines.push(blockText(ch[i]));
    return lines.join('\n');
  }
  // 写入校验比较:两侧都用块级规范文本(captureText)比对,**保留全部空行(含前导/尾随)**:只归一
  // `\r` + 去各行尾的水平空白(空格/Tab,用户多半无意、PM 可能规整),**绝不折叠或裁剪任何 `\n`**。
  // 折叠/裁剪换行会让 `a\n\nb`→`a\nb`、或丢前导空行也判通过 → 消费/发送被破坏草稿(见 review r5/r6)。
  function sameText(a, b) {
    function f(s) { return (s == null ? '' : String(s)).replace(/\r/g, '').replace(/[ \t]+$/gm, ''); }
    return f(a) === f(b);
  }
  function fiberOf(el) { if (!el) return null; for (var k in el) { if (k.indexOf('__reactFiber$') === 0) return el[k]; } return null; }
  // 把文本合成 text/plain paste 进 composer(ProseMirror 的 paste 路径把 \n→hard break、\n\n→段落,
  // 是多行最可靠的注入方式,与图片合成 paste 同机制)。
  function pasteText(pm, text) {
    try {
      var dt = new DataTransfer(); dt.setData('text/plain', text);
      var ev = new ClipboardEvent('paste', { bubbles: true, cancelable: true });
      Object.defineProperty(ev, 'clipboardData', { value: dt });
      pm.dispatchEvent(ev);
    } catch (x) {}
  }
  // 设输入框内容:focus → 全选删除(CDP 实证可靠清空)→ 写入(空则只清)。**返回是否真落值**
  // (caller 据此决定是否消费 stash,绝不在没落值时丢草稿):目标空 → 须真空;目标非空 → 块级规范
  // 文本须与目标一致(sameText,**保留空行**;不折叠换行,否则丢空行也判通过 → 消费被破坏草稿)。
  // **写入机制实证(CDP)**:本 composer 的 Shift+Enter = 新建 `<p>` 段落(非 `<br>` hard break),
  // 多行草稿即多个 `<p>`。text/plain 合成 paste 恰好按 `\n` 切段、重建同构 `<p>`,**稳定、与原草稿
  // 同构、空行不丢**,故作首选。execCommand insertLineBreak 会插入裸 `\n` 被 PM 规整成空格(丢换行),
  // **不可用**;回退仅用 insertText / InputEvent。校验用块级 captureText(与捕获/存储同一空间)。
  function setComposer(text) {
    var e = composerEl(); if (!e) return false;
    try { e.focus(); } catch (x) {}
    try { document.execCommand('selectAll', false, null); } catch (x) {}
    try { document.execCommand('delete', false, null); } catch (x) {}
    if (text) {
      pasteText(e, text); // 首选:多行安全(重建 <p> 段落,与 Shift+Enter 同构,空行保留)
      if (!sameText(captureText(), text)) { // 回退 1:execCommand insertText(paste 被拦时;单行可靠)
        try { document.execCommand('selectAll', false, null); document.execCommand('delete', false, null); } catch (x) {}
        try { document.execCommand('insertText', false, text); } catch (x) {}
      }
      if (!sameText(captureText(), text)) { // 回退 2:单次 InputEvent
        try {
          e.dispatchEvent(new InputEvent('beforeinput', { inputType: 'insertText', data: text, bubbles: true, cancelable: true }));
          e.dispatchEvent(new InputEvent('input', { inputType: 'insertText', data: text, bubbles: true }));
        } catch (x) {}
      }
    }
    var now = captureText();
    return text ? (now.trim() !== '' && sameText(now, text)) : (now.trim() === '');
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

  // ── 图片附件(CDP 实证:附件行上方 fiber props 持有 imageAttachments + onRemoveImage)──
  // 从附件行 / composer 往上爬 fiber,拿持有 imageAttachments 数组的 props。无附件时附件行不渲染
  // → 返回 null(视作无图)。
  function composerAtt() {
    var anchor = document.querySelector('[data-composer-attachments-row]') || composerEl();
    if (!anchor) return null;
    var el = anchor, f = null; while (el && !(f = fiberOf(el))) el = el.parentElement;
    var d = 0;
    while (f && d < 40) {
      var p = f.memoizedProps;
      if (p && typeof p === 'object' && Array.isArray(p.imageAttachments)) {
        return { images: p.imageAttachments, onRemoveImage: typeof p.onRemoveImage === 'function' ? p.onRemoveImage : null };
      }
      f = f.return; d++;
    }
    return null;
  }
  // 读当前图片为可序列化 {src(dataURL), filename}(只留 dataURL 的,能恢复的才存)
  function readImages() {
    var a = composerAtt(); if (!a || !a.images) return [];
    return a.images
      .filter(function (im) { return im && typeof im.src === 'string' && im.src.indexOf('data:') === 0; })
      .map(function (im) { return { src: im.src, filename: im.filename || 'image.png' }; });
  }
  // 清掉 composer 里**可序列化的(data:)**图片(那些已被 readImages 存进 stash 的);**不动**非 data:
  // 图片(如 blob 上传)—— readImages 存不了它们,这里也不删,避免「没存上又删掉 = 丢图」(见 review)。
  // 也**不动文件**(文件不在 stash 范围,见 CAT-260)。
  function clearImages() {
    var a = composerAtt(); if (!a || !a.onRemoveImage || !a.images) return;
    a.images.forEach(function (im) {
      if (im && typeof im.src === 'string' && im.src.indexOf('data:') === 0) {
        try { a.onRemoveImage(im.id); } catch (e) {}
      }
    });
  }
  // 把保存的图片合成 paste 回 composer(atob 解 base64→File→ClipboardEvent;fetch(dataURL) 被 CSP 挡)
  function addImages(imgs) {
    if (!imgs || !imgs.length) return;
    var pm = composerEl(); if (!pm) return;
    try { pm.focus(); } catch (e) {}
    imgs.forEach(function (im) {
      try {
        var arr = im.src.split(','), mime = (arr[0].match(/:(.*?);/) || [])[1] || 'image/png';
        var bstr = atob(arr[1] || ''), n = bstr.length, u8 = new Uint8Array(n);
        while (n--) u8[n] = bstr.charCodeAt(n);
        var file = new File([u8], im.filename || 'image.png', { type: mime });
        var dt = new DataTransfer(); dt.items.add(file);
        var ev = new ClipboardEvent('paste', { bubbles: true, cancelable: true });
        Object.defineProperty(ev, 'clipboardData', { value: dt });
        pm.dispatchEvent(ev);
      } catch (e) {}
    });
  }
  // 合成 paste 是异步落值(FileReader+persist),发送前轮询图片就位(最多 ~2.5s)。
  // **回调带 ok**:就位 → cb(true);超时 → cb(false)。caller 必须区分 —— 超时不能照发(会发出
  // 不带图片的消息),更不能在确认前消费 stash。
  function waitImages(target, cb) {
    var tries = 0;
    var iv = setInterval(function () {
      tries++;
      if (readImages().length >= target) { clearInterval(iv); cb(true); }
      else if (tries > 25) { clearInterval(iv); cb(false); }
    }, 100);
  }
  // composer 是否还挂着**任何**附件(图片或文件)。附件行 `[data-composer-attachments-row]` 是
  // **条件渲染**的:有任意附件才在 DOM,空态整行移除(CDP 实证)。故它在场 = 还有附件。比只数
  // imageAttachments 更全 —— **也能挡住文件**:文件不在 stash 范围(curEntry 不存、clearImages
  // 不清,见 CAT-260),但 send 不该把残留文件一起发出(见 review),所以清空判定必须把它算进去。
  function hasAttachments() { return !!document.querySelector('[data-composer-attachments-row]'); }
  // composer 是否挂着**非图片(文件)**附件。附件 chip 是 `.composer-attachment-surface`;图片 chip
  // 内有 `<img>`,文件 chip 没有(CDP 实证图片 chip 含 img)。文件不可序列化/恢复(CAT-260):
  // push 遇到文件应中止、保草稿,别只存文字把文件孤儿化(见 review)。
  function hasFileAttachment() {
    var chips = document.querySelectorAll('[data-composer-attachments-row] .composer-attachment-surface');
    for (var i = 0; i < chips.length; i++) { if (!chips[i].querySelector('img')) return true; }
    return false;
  }
  // 图片恢复/发送的完整时序(确保只带对的图、且写失败/未就位时不丢暂存),纯文本也走它(等旧附件清空):
  // 1) 先等当前附件(图片**和**文件)全部清空(clearImages 异步;否则旧图仍在 React 态、或残留
  //    文件,submit 会把它们一起发出,见 review)——附件行消失才 2);文件清不掉 → 超时安全中止。
  // 2) paste 目标图(纯文本时 imgs=[] → 无操作);3) 等目标图就位(imgs=[] 时 waitImages(0) 立即 true)。
  // 成功 cb(true);清空或就位超时 cb(false)(caller 据此不消费/不发)。
  function settleImages(imgs, cb) {
    var n = 0;
    var iv = setInterval(function () {
      n++;
      if (!hasAttachments()) { clearInterval(iv); addImages(imgs); waitImages(imgs.length, cb); }
      else if (n > 25) { clearInterval(iv); cb(false); } // 旧附件清空超时(如残留文件)= 失败,不消费/不发
    }, 100);
  }
  // 最小 toast(暂存失败/降级时告知;无侵入,2.5s 自removed)
  function notify(msg) {
    try {
      var t = el('div', null, msg); t.id = 'cat-stash-toast';
      t.style.cssText = 'position:fixed;left:50%;bottom:80px;transform:translateX(-50%);z-index:99999;padding:8px 14px;border-radius:8px;background:rgba(20,24,36,.92);color:#fff;font-size:13px;box-shadow:0 6px 24px rgba(0,0,0,.4);pointer-events:none';
      var old = document.getElementById('cat-stash-toast'); if (old) old.remove();
      document.body.appendChild(t);
      setTimeout(function () { try { t.remove(); } catch (e) {} }, 2500);
    } catch (e) {}
  }

  // ── 核心动作 ──
  // restore/sendItem 共享 composer 且异步(settleImages):全局串行锁,防同条重复点 / 不同条并发 /
  // 处理期间 push 互相 clobber composer + 重复提交(见 review)。每条路径(含早退)必复位。
  var __busy = false;
  // 当前 composer 内容(文本 + 图片)打包成一条 swap 用的 entry;全空返 null
  function curEntry() {
    var t = captureText(), imgs = readImages();
    if ((!t || !t.trim()) && !imgs.length) return null;
    return { id: uid(), text: t || '', ts: Date.now(), images: imgs };
  }
  // 当前内容 swap 进 stash 但**暂不消费目标项**:先存一份「原列表 + cur」(cur 不丢),目标项
  // 仍在列表里;真正落值/发送成功后再单独移除目标项。返回该 swap 是否 save 成功(配额够)。
  function swapInCurrent(arr) {
    var cur = curEntry();
    if (!cur) return true;          // 输入框空,无需 swap
    var next = arr.slice(); next.push(cur);
    return save(next);
  }
  function consumeItem(id) { save(load().filter(function (x) { return x.id !== id; })); }
  // 恢复:把目标(文本+图片)放进 composer;输入框非空则先把当前内容 swap 进 stash(不丢)。
  // **先 swap-save(配额够)再动 composer**——保存失败就安全中止。**setComposer 真落值才消费目标项**;
  // 有图时**进一步等图片真就位**才消费(写/paste 失败 → 目标项保留在 stash,绝不出现「消费了却没
  // 完整落到输入框」)。
  function restore(id) {
    if (__busy) return;            // 处理中,忽略重复点(防 double restore/send,见 review)
    var arr = load(); var item = findItem(arr, id); if (!item) return;
    __busy = true;
    if (!swapInCurrent(arr)) { __busy = false; notify('暂存空间不足,恢复已取消'); return; } // composer 还没动,安全
    clearImages();                 // 清掉当前 data: 图片(已 swap 进 stash);不动非 data: / 文件
    if (!setComposer(item.text)) { __busy = false; notify('写入输入框失败,恢复已取消'); closeMenu(); ensure(); return; } // 目标项仍在 stash
    // 纯文本也走 settleImages([]):先等旧附件清空再消费(避免恢复后还残留上一稿的图/文件);有图则等 paste 就位。
    settleImages(item.images || [], function (ok) {
      if (ok) consumeItem(id); else notify('图片未就位,已保留暂存');
      __busy = false; closeMenu(); ensure();
    });
  }
  // 发送:同 restore 把目标放进 composer。**仅在文本落值 + 图片就位后才消费并提交**:
  // 写失败 → 不消费、不提交;图片超时未就位 → 不消费、不提交(避免发出不带图片的消息),目标项
  // 留在 stash 可重试。失败均无损(当前内容已 swap 进 stash,目标项仍在)。
  function sendItem(id) {
    if (__busy) return;            // 处理中,忽略重复点(防重复发送,见 review)
    var arr = load(); var item = findItem(arr, id); if (!item) return;
    __busy = true;
    if (!swapInCurrent(arr)) { __busy = false; notify('暂存空间不足,发送已取消'); return; }
    clearImages();
    if (!setComposer(item.text)) { __busy = false; notify('写入输入框失败,发送已取消'); ensure(); return; } // 目标项仍在 stash
    // 纯文本也走 settleImages([]):**先等旧附件清空再提交**,否则上一稿的图/文件(异步未清完)会被一起
    // 发出(见 review);有图则等目标图 paste 就位。**提交成功才消费**(submitComposer 返回 false——按钮/
    // fiber 路径变化或暂不可提交——则保留暂存,不静默吞掉,见 review);失败均保留可重试。
    settleImages(item.images || [], function (ok) {
      if (!ok) notify('图片未就位,已保留暂存(未发送)');
      else if (submitComposer()) consumeItem(id);
      else notify('发送失败,已保留暂存');
      __busy = false; ensure();
    });
  }
  function del(id) { save(load().filter(function (x) { return x.id !== id; })); ensure(); }
  // 暂存当前(文本+图片)→ 清空输入框(**不动文件**,文件不在范围见 CAT-260)。
  // **先 save 成功才清**,避免清了没存上丢内容;配额溢出时降级为只存文字(图片太大),不丢文字。
  // load() 返回只读缓存,故先 .slice() 再 push(不污染缓存)。
  function stashCurrent() {
    if (__busy) return;            // restore/send 异步进行中,别动 composer(见 review)
    var t = captureText(), imgs = readImages();
    if ((!t || !t.trim()) && !imgs.length) return;
    // 含文件附件:中止 push,保草稿不动 —— 文件无法暂存/恢复(CAT-260),只存文字会把文件孤儿化、
    // 被下一条原生发送带走(见 review)。提示用户先移除文件。
    if (hasFileAttachment()) { notify('草稿含文件附件,暂不支持暂存(请先移除文件)'); return; }
    var entry = { id: uid(), text: t || '', ts: Date.now(), images: imgs };
    var arr = load().slice(); arr.push(entry);
    if (!save(arr)) {
      if (t && t.trim()) { // 退化:只存文字,图片留在输入框不清
        var a2 = load().slice(); a2.push({ id: entry.id, text: entry.text, ts: entry.ts, images: [] });
        if (save(a2)) { setComposer(''); notify('图片过大未暂存,仅暂存了文字'); ensure(); return; }
      }
      notify('暂存失败:存储空间不足'); return; // 什么都没存,什么都不动
    }
    if (t && t.trim()) setComposer(''); // 存成功才清:文字 + 图片(文件保留)
    clearImages();
    // 刚暂存若含图:等附件真正清空再解锁(clearImages 异步;否则刚暂存的图还挂着,用户紧接着发送的
    // 插话会把它一起带出去,见 review)。__busy 期间我方 push/restore/send 不可再入。
    if (imgs.length) {
      __busy = true;
      var n = 0;
      var iv = setInterval(function () { n++; if (!hasAttachments() || n > 25) { clearInterval(iv); __busy = false; ensure(); } }, 100);
    }
    ensure();
  }

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
      // 行内图片计数 chip
      '#cat-stash-entry .csimg{flex:0 0 auto;display:inline-flex;align-items:center;gap:3px;font-size:11px;color:var(--color-token-text-tertiary,rgba(238,241,247,.5));font-variant-numeric:tabular-nums}' +
      '#cat-stash-entry .csimg svg{width:13px;height:13px}' +
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
    del: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2"/><path d="M19 6l-1 14a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1L5 6"/></svg>',
    image: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="8.5" cy="8.5" r="1.5"/><path d="M21 15l-5-5L5 21"/></svg>'
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
  function renderPanel(node, arr) {
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
        var nimg = (it.images || []).length;
        var label = (it.text || '').replace(/\s+/g, ' ').trim();
        if (!label && nimg) label = '(图片)';
        var t = el('div', 'cstext', label); t.title = it.text || '';
        row.appendChild(t);
        if (nimg) {
          var chip = el('span', 'csimg'); chip.innerHTML = ICON.image;
          chip.appendChild(el('span', null, String(nimg))); chip.title = nimg + ' 张图片';
          row.appendChild(chip);
        }
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
    // 廉价签名:条目不可变(只增删/消费),id 列表 + 各自图片数即可表征变化;**不** stringify 整个
    // load()(图片 dataURL 可达数 MB,每 tick stringify 会很热)。load() 已带解析缓存(见上),
    // 此处与 renderPanel 共用同一份(不重复解析)。
    var arr = load();
    var sig = arr.map(function (e) { return e.id + '#' + ((e.images || []).length); }).join(',');
    if (fresh || sig !== window.__catStashSig) { renderPanel(node, arr); window.__catStashSig = sig; }
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
      var lbl = (it.text || '').replace(/\s+/g, ' ').trim();
      var ni = (it.images || []).length;
      if (!lbl && ni) lbl = '(图片)';
      if (ni) lbl += ' ·图' + ni;
      mi.appendChild(el('span', 't', lbl));
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
    // 状态刷新(仅变化时写,避免自触发 observer churn)。load() 带解析缓存,热路径不重复 parse。
    var arr = load();
    // 有文字或有图片(均走廉价检查:文字用 textContent 判空而非 innerText 触发 reflow;图片查
    // 附件行里的 img 缩略图,避免每 tick 爬 fiber)即可暂存
    var hasImg = !!document.querySelector('[data-composer-attachments-row] img');
    var pushDisabled = !(composerTextRaw().trim()) && !hasImg;
    if (bar.__push.disabled !== pushDisabled) bar.__push.disabled = pushDisabled;
    bar.__push.title = '暂存当前输入(文字 + 图片)';
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
