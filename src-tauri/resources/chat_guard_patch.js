// [MOC-323] Codex/ChatGPT 桌面 app 的 Chat(经典 ChatGPT 对话)auth 守卫补丁。
//
// 背景:app 主进程(.vite/build/main-BOrkpaTV.js)有 `isDesktopAuthAllowedUrl(url)`,
// 只对白名单 host(localhost / localhost:8000 / *.openai.com / *.chatgpt.com)附加
// ChatGPT 鉴权;transfer 把 `CODEX_API_BASE_URL` 指向本地 proxy(非白名单端口)时,
// `/f/conversation` 会被守卫以 `Refusing to attach authentication to non-OpenAI URL`
// 拒发。守卫在**主进程** JS,renderer 的 `--remote-debugging-port` 够不到,故经
// `NODE_OPTIONS=--require <本文件>` 在主进程启动早期注入,hook CommonJS 编译,给守卫
// 白名单补上 transfer 的 host(不改磁盘 app.asar、不重打包、不动其它逻辑)。
//
// 端口经 env `CAS_CHAT_GUARD_HOST` 传入(形如 `localhost:18080`),缺省不打补丁(安全)。
'use strict';
try {
  const Module = require('module');
  const extraHost = (process.env.CAS_CHAT_GUARD_HOST || '').trim().toLowerCase();
  // 只接受 `localhost:<port>` 形态,避免误放宽成任意 host。
  if (/^localhost:\d{2,5}$/.test(extraHost)) {
    const NEEDLE = 'n===`localhost:8000`';
    const REPLACEMENT = '(n===`localhost:8000`||n===`' + extraHost + '`)';
    const origCompile = Module.prototype._compile;
    Module.prototype._compile = function (content, filename) {
      if (
        typeof content === 'string' &&
        content.indexOf('isDesktopAuthAllowedUrl') !== -1 &&
        content.indexOf(NEEDLE) !== -1
      ) {
        // split/join 全量替换(该字面量在守卫函数内唯一出现)。
        content = content.split(NEEDLE).join(REPLACEMENT);
        Module.prototype._compile = origCompile; // 命中后即卸载 hook,零后续开销。
      }
      return origCompile.call(this, content, filename);
    };
  }
} catch (_e) {
  // 补丁失败不能拖垮 app 启动 —— 静默降级(Chat 自定义模型不可用,但 app 正常)。
}
