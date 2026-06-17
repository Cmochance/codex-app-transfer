import { api } from './http'

export interface ProxyLogEntry {
  at: string
  level: string
  message: string
}

// 移植 api.js mapLog
function mapLog(log: { time: string; level: string; message: string }): ProxyLogEntry {
  return { at: log.time, level: (log.level || '').toLowerCase(), message: log.message }
}

export const startProxy = () => api('POST', '/api/proxy/start')
export const stopProxy = () => api('POST', '/api/proxy/stop')
export const getProxyStatus = () =>
  api<{ running?: boolean; port?: number } & Record<string, unknown>>('GET', '/api/proxy/status')
export async function getProxyLogs(): Promise<ProxyLogEntry[]> {
  const data = await api<{ logs?: { time: string; level: string; message: string }[] }>(
    'GET',
    '/api/proxy/logs',
  )
  return (data.logs || []).map(mapLog)
}
export const clearProxyLogs = () => api('POST', '/api/proxy/logs/clear')
