import { api } from './http'

// MOC-256 webFetch headless 后端门控:选 auto/headless 时先探测系统 Chrome,无则按需下载。

// GET /api/chrome/ready — readiness gate(对齐后端 chrome_ready_without_download):已下载内置 shell
// 或系统 Chrome `--version` 自检通过 → ready,且都不触发下载。门控用它而非 detect:detect 只查系统
// Chrome 文件存在,忽略已下载 shell、也不做自检(stale/坏 Chrome 会被误判命中后 launch 时静默下载)。
export const getChromeReady = () => api<{ ready?: boolean }>('GET', '/api/chrome/ready')

// POST /api/chrome/ensure — 确保 chrome-headless-shell 就绪(系统无 Chrome 时按需下载 ~86MB,阻塞 ~20s)。
export const ensureChrome = () =>
  api<{ success?: boolean; path?: string; message?: string }>('POST', '/api/chrome/ensure')

// GET /api/system-proxy/status — MOC-114 系统代理(梯子)连通性探测。
// 门控 auto/headless 前查:配了梯子但连不上 → 抓不到墙外站 → 降级 wreq。
export interface SystemProxyStatus {
  configured?: boolean
  connected?: boolean
  kind?: string
}
export const getSystemProxyStatus = () =>
  api<{ success?: boolean; systemProxy?: SystemProxyStatus }>('GET', '/api/system-proxy/status')
