// [MOC-323] Codex/ChatGPT 桌面 app 的 Chat(经典 ChatGPT 对话)auth 守卫补丁。
//
// 背景:app 主进程(.vite/build/main-*.js)有 `isDesktopAuthAllowedUrl(url)`,只对白名单
// host(localhost / localhost:8000 / *.openai.com / *.chatgpt.com)附加 ChatGPT 鉴权;
// transfer 把 `CODEX_API_BASE_URL` 指向本地 proxy(非白名单端口)时,`/f/conversation` 会被
// 守卫以 `Refusing to attach authentication to non-OpenAI URL` 拒发。守卫在**主进程** JS,
// renderer 的 `--remote-debugging-port` 够不到,故经 `NODE_OPTIONS=--require <本文件>` 在主
// 进程启动早期注入,hook CommonJS 编译,给守卫白名单补上 transfer 的 host(不改磁盘 app.asar、
// 不重打包、不动其它逻辑)。
//
// 端口经 env `CAS_CHAT_GUARD_HOST` 传入(形如 `localhost:18080`),缺省不打补丁(安全)。
//
// **落地校验(code-review C2)**:补丁靠字符串匹配 minified 第三方代码,Codex 更新可能让守卫
// 形态漂移导致**静默失效**。故:① 用 regex 捕获守卫里的变量名(不写死 `n`,容忍 minifier 改名);
// ② 把结果(applied / reason)写 `~/.codex-app-transfer/chat_guard_status.json` breadcrumb,让
// 上层(transfer daemon)能发现「守卫没打上」并告警,而非无声。
'use strict';

function writeStatus(obj) {
  try {
    const fs = require('fs');
    const path = require('path');
    const os = require('os');
    const dir = path.join(os.homedir(), '.codex-app-transfer');
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(
      path.join(dir, 'chat_guard_status.json'),
      JSON.stringify(Object.assign({ ts: Date.now() }, obj))
    );
  } catch (_e) {
    // 连 breadcrumb 都写不了也不能拖垮启动。
  }
}

try {
  const Module = require('module');
  const extraHost = (process.env.CAS_CHAT_GUARD_HOST || '').trim().toLowerCase();
  // 只接受 `localhost:<port>` 形态,避免误放宽成任意 host。
  if (!/^localhost:\d{2,5}$/.test(extraHost)) {
    writeStatus({ applied: false, reason: 'no-guard-host-env' });
  } else {
    // **先写初始 pending 失败态**(code-review [4]):若未来 Codex 更新把守卫函数改名/移走,
    // 下面的 hook 永远进不了替换块、不会写新 status,旧 `applied:true` 会残留、骗过 daemon 判「健康」。
    // 先落 pending,hook 真触发(命中/未命中)再覆盖;永不触发则保持失败态 → 可被发现。
    writeStatus({ applied: false, reason: 'pending-guard-module-not-compiled' });
    // 容忍 minified 变量名漂移:捕获 `<var>===`localhost:8000`` 的变量名,不写死 `n`。
    const GUARD_RE = /([A-Za-z_$][\w$]*)===`localhost:8000`/g;
    const origCompile = Module.prototype._compile;
    let done = false;
    Module.prototype._compile = function (content, filename) {
      if (!done && typeof content === 'string' && content.indexOf('isDesktopAuthAllowedUrl') !== -1) {
        done = true;
        Module.prototype._compile = origCompile; // 处理完守卫模块即卸载 hook,零后续开销。
        const replaced = content.replace(GUARD_RE, function (_m, v) {
          return '(' + v + '===`localhost:8000`||' + v + '===`' + extraHost + '`)';
        });
        if (replaced !== content) {
          content = replaced; // 全量替换(该字面量在守卫函数内的多处都放宽)。
          writeStatus({ applied: true, host: extraHost });
        } else {
          // 守卫函数在、但目标字面量不在 → 很可能 Codex 更新改了守卫形态,补丁需跟进。
          writeStatus({ applied: false, reason: 'needle-not-found' });
        }
      }
      return origCompile.call(this, content, filename);
    };
  }
} catch (e) {
  // 补丁失败不能拖垮 app 启动 —— 降级,但把原因写 breadcrumb 供发现(不再无声)。
  writeStatus({ applied: false, reason: 'exception:' + String((e && e.message) || e) });
}
