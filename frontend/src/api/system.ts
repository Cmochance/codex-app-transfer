import { api } from './http'

// 软件版本 / 检查更新 / 打开外链(系统浏览器)
export const getAppVersion = () => api<{ version?: string }>('GET', '/api/version')
export const checkAppUpdate = () =>
  api<{ hasUpdate?: boolean; latestVersion?: string; currentVersion?: string }>(
    'GET',
    '/api/update/check',
  )
export const openExternalUrl = (url: string) =>
  api<{ success?: boolean }>('POST', '/api/open-url', { url })
