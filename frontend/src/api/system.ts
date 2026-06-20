import { api } from './http'

// 软件版本 / 检查更新 / 打开外链(系统浏览器)
export const getAppVersion = () => api<{ version?: string }>('GET', '/api/version')
export const checkAppUpdate = () =>
  api<{
    // 后端权威字段(update.rs check_update_impl):是否有更新 + 当前平台是否支持 in-app 安装。
    updateAvailable?: boolean
    installSupported?: boolean
    latestVersion?: string
    currentVersion?: string
  }>('GET', '/api/update/check')
// 下载并安装更新:后端做 macOS translocation 预检 → 下载 installer → app 退出拉起安装器。
// 无 body(后端默认 url/current/platform)。成功后 app 即将退出,故返回多为 best-effort。
export const installAppUpdate = () =>
  api<{ success?: boolean; installerStarted?: boolean; message?: string }>(
    'POST',
    '/api/update/install',
  )
export const openExternalUrl = (url: string) =>
  api<{ success?: boolean }>('POST', '/api/open-url', { url })

// [MOC-261 一-11] transfer adapter 静默丢弃的未知 Responses API 工具类型累计计数(本进程累计、
// 重启归零)。某类型计数持续增长 = 上游新增工具类型被静默丢弃的金丝雀(MOC-32 那类 bug 的征兆)。
export interface DroppedToolsStatus {
  total: number
  by_type: Record<string, number>
}
export const getDroppedTools = () =>
  api<DroppedToolsStatus>('GET', '/api/diagnostic/dropped-tools')

// 反馈提交(接旧版 /api/feedback worker;body 必填,include_diagnostics 默认 true)
export interface FeedbackPayload {
  title?: string
  contact_email?: string
  body: string
  include_diagnostics?: boolean
}
export const submitFeedback = (payload: FeedbackPayload) =>
  api<{ id?: string }>('POST', '/api/feedback', payload)
