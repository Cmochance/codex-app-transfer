import { api } from './http'

export interface ProxyLogEntry {
  at: string
  level: string
  message: string
}
export interface ProxyStats {
  total: number
  success: number
  failed: number
  today: number
}

// 移植 api.js mapLog
function mapLog(log: { time: string; level: string; message: string }): ProxyLogEntry {
  return { at: log.time, level: (log.level || '').toLowerCase(), message: log.message }
}

// 启动转发, 可带端口(老 api.js startProxy(port):port 时发 {port},否则用配置端口)
export const startProxy = (port?: number) =>
  api('POST', '/api/proxy/start', port ? { port: Number(port) } : undefined)
export const stopProxy = () => api('POST', '/api/proxy/stop')
export const getProxyStatus = () =>
  api<{ running?: boolean; port?: number; stats?: ProxyStats }>('GET', '/api/proxy/status')
export async function getProxyLogs(): Promise<ProxyLogEntry[]> {
  const data = await api<{ logs?: { time: string; level: string; message: string }[] }>(
    'GET',
    '/api/proxy/logs',
  )
  return (data.logs || []).map(mapLog)
}
export const clearProxyLogs = () => api('POST', '/api/proxy/logs/clear')
export const openProxyLogDir = () => api('POST', '/api/proxy/logs/open-dir')
