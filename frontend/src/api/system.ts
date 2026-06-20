import { api } from './http'

// 软件版本 / 检查更新 / 打开外链(系统浏览器)
export const getAppVersion = () => api<{ version?: string }>('GET', '/api/version')
export const checkAppUpdate = () =>
  api<{ hasUpdate?: boolean; latestVersion?: string; currentVersion?: string }>(
    'GET',
    '/api/update/check',
  )
// 下载并安装更新:后端做 macOS translocation 预检 → 下载 installer → app 退出拉起安装器。
// 无 body(后端默认 url/current/platform)。成功后 app 即将退出,故返回多为 best-effort。
export const installAppUpdate = () =>
  api<{ success?: boolean; installerStarted?: boolean; message?: string }>(
    'POST',
    '/api/update/install',
  )
export const openExternalUrl = (url: string) =>
  api<{ success?: boolean }>('POST', '/api/open-url', { url })

// 反馈提交(接旧版 /api/feedback worker;body 必填,include_diagnostics 默认 true)
export interface FeedbackPayload {
  title?: string
  contact_email?: string
  body: string
  include_diagnostics?: boolean
}
export const submitFeedback = (payload: FeedbackPayload) =>
  api<{ id?: string }>('POST', '/api/feedback', payload)
