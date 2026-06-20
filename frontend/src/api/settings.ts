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

// 配置文件:导出整份配置(providers + 模型映射 + 全部设置,含 API key 明文)到文件 /
// 从文件导入(后端自动 before-import 备份 + 保留现有 provider secret + normalize 校验)。
export const exportConfig = () =>
  api<{ format: string; exportedAt: string; config: unknown }>('GET', '/api/config/export')
export const importConfig = (data: unknown) =>
  api<{ success?: boolean; message?: string }>('POST', '/api/config/import', data)
