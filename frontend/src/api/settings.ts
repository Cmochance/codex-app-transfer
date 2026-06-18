import { api } from './http'

// /api/settings 是一个自由 key-value 对象(theme/language/各开关/端口/webFetchBackend/updateUrl…)。
// GET 返裸 settings 对象;PUT 浅合并传入的 partial,返 {success, settings(合并后), webFetchSyncWarning?}。
export type Settings = Record<string, unknown>

export const getSettings = () => api<Settings>('GET', '/api/settings')

export async function saveSettings(
  partial: Settings,
): Promise<{ settings: Settings; webFetchSyncWarning?: string }> {
  const data = await api<{ settings?: Settings; webFetchSyncWarning?: string }>(
    'PUT',
    '/api/settings',
    partial,
  )
  return { settings: data.settings || {}, webFetchSyncWarning: data.webFetchSyncWarning }
}
