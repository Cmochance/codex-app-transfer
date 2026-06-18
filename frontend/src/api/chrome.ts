import { api } from './http'

// MOC-256 webFetch headless 后端门控:选 auto/headless 时先探测系统 Chrome,无则按需下载。

// GET /api/chrome/detect — 探测系统已装 Chrome/Edge/Chromium(不下载),命中返路径。
export const detectChrome = () =>
  api<{ detected?: boolean; path?: string }>('GET', '/api/chrome/detect')

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
