// Codex Desktop 半区 typed API 层 — 收编旧 app.js / api.js 的 desktop / theme /
// residual / snapshot / trace-viewer fetch(逐字保留路径/body/响应字段)。
// 后端 err() 返回 {success:false, error, message},故统一走 api() wrapper。
import { api } from './http'

// ───────────────────────────────────────────────────────────────────────────
// Theme(Codex Desktop 皮肤注入,/api/desktop/theme/*)
// ───────────────────────────────────────────────────────────────────────────
export interface ThemeEntry {
  id: string
  displayNameZh: string
  displayNameEn: string
  hasMascot: boolean
  previewDataUri: string
}

// ⚠️ serde externally-tagged(PascalCase,**绝不能改 camelCase**,PR#265 踩过):
//   "Disabled" / "Applying" / {Applied:{theme_id}} / {Failed:{error}}
export type ThemeStatus =
  | 'Disabled'
  | 'Applying'
  | { Applied: { theme_id: string } }
  | { Failed: { error: string } }

export function themeList() {
  return api<{ themes?: ThemeEntry[] }>('GET', '/api/desktop/theme/list')
}
export function themeStatus() {
  return api<{ status: ThemeStatus }>('GET', '/api/desktop/theme/status')
}
export function themeApply(themeId: string) {
  return api('POST', '/api/desktop/theme/apply', { theme_id: themeId })
}
export function themeUploadCustom(dataUri: string) {
  return api('POST', '/api/desktop/theme/custom/upload', { data_uri: dataUri })
}
export function themeDeleteCustom() {
  return api('DELETE', '/api/desktop/theme/custom')
}
export function restartCodexApp() {
  return api('POST', '/api/desktop/restart-codex-app')
}

// ───────────────────────────────────────────────────────────────────────────
// Desktop 配置(Codex CLI 接管,/api/desktop/{status,configure,clear})
// ───────────────────────────────────────────────────────────────────────────
export interface DesktopHealth {
  needsApply: boolean
  issues: { message?: string }[]
}
export interface DesktopConfig {
  inferenceProvider: string
  inferenceGatewayBaseUrl: string
  inferenceGatewayApiKey: string
  inferenceGatewayAuthScheme: string
  inferenceModels: string
}
export interface DesktopStatus {
  configured: boolean
  health: DesktopHealth
  config: DesktopConfig
}

// 旧 api.js getDesktopStatus:二次组装 /api/desktop/status + /api/status(取 proxyPort
// 作 baseUrl 兜底),API Key 掩码成 '******'。
export async function getDesktopStatus(): Promise<DesktopStatus> {
  const data = await api<{
    configured?: boolean
    health?: DesktopHealth
    keys?: Partial<DesktopConfig>
  }>('GET', '/api/desktop/status')
  const status = await api<{ proxyPort?: number }>('GET', '/api/status')
  const proxyPort = status.proxyPort || 18080
  const k = data.keys || {}
  return {
    configured: !!data.configured,
    health: data.health || { needsApply: false, issues: [] },
    config: {
      inferenceProvider: k.inferenceProvider || 'gateway',
      inferenceGatewayBaseUrl: k.inferenceGatewayBaseUrl || `http://127.0.0.1:${proxyPort}`,
      inferenceGatewayApiKey: k.inferenceGatewayApiKey ? '******' : '',
      inferenceGatewayAuthScheme: k.inferenceGatewayAuthScheme || 'bearer',
      inferenceModels: k.inferenceModels || '[]',
    },
  }
}

export function configureDesktop() {
  return api<{ commands?: { temporary?: string } }>('POST', '/api/desktop/configure')
}
export function clearDesktop() {
  return api<{ restored?: boolean }>('POST', '/api/desktop/clear')
}

// ───────────────────────────────────────────────────────────────────────────
// Residual 反投毒自检(#268,/api/desktop/{scan,repair}-residual)
// ───────────────────────────────────────────────────────────────────────────
export type ResidualKind = 'liveConfig' | 'activeSnapshot' | 'recoverySnapshot'
export interface PollutedFile {
  path: string
  kind: ResidualKind
  matchedSignatures: string[]
  fieldsToStrip: string[]
}
export interface ResidualScanReport {
  polluted: PollutedFile[]
  transferCurrentlyApplied: boolean
}
export interface RepairedFile {
  path: string
  kind: ResidualKind
  strippedKeys: string[]
}
export interface RepairResult {
  success: boolean
  scan: ResidualScanReport
  repair: { repaired: RepairedFile[]; dryRun: boolean }
}

export function scanResidualPollution() {
  return api<ResidualScanReport>('GET', '/api/desktop/scan-residual')
}
export function repairResidualPollution(dryRun = false) {
  return api<RepairResult>('POST', '/api/desktop/repair-residual', { dryRun })
}

// ───────────────────────────────────────────────────────────────────────────
// Snapshot 恢复(/api/desktop/{snapshot-status,snapshots,restore})
// ───────────────────────────────────────────────────────────────────────────
export type SnapshotKind = 'active' | 'recovery' | 'legacy'
export interface SnapshotStatus {
  hasSnapshot: boolean
  snapshotAt?: string
  configExisted?: boolean
  authExisted?: boolean
  appVersion?: string
  restorableCount: number
}
export interface SnapshotInfo {
  id: string
  kind: SnapshotKind
  snapshotAt?: string
  configExisted?: boolean
  authExisted?: boolean
  appVersion?: string
  providerName: string | null
}

export function getDesktopSnapshotStatus() {
  return api<SnapshotStatus>('GET', '/api/desktop/snapshot-status')
}
export async function getDesktopSnapshots(): Promise<SnapshotInfo[]> {
  const data = await api<{ snapshots?: SnapshotInfo[] }>('GET', '/api/desktop/snapshots')
  return data.snapshots || []
}
export function restoreDesktopSnapshot(snapshotId: string) {
  return api<{ restored?: boolean }>('POST', '/api/desktop/restore', {
    snapshotId,
    cleanupAll: true,
  })
}

// ───────────────────────────────────────────────────────────────────────────
// Trace viewer 诊断(MOC-185,session 级,/api/trace-viewer/*,固定端口 18090)
// ───────────────────────────────────────────────────────────────────────────
export function traceViewerStatus() {
  return api<{ running: boolean; url: string | null }>('GET', '/api/trace-viewer/status')
}
export function traceViewerStart() {
  return api<{ url?: string }>('POST', '/api/trace-viewer/start')
}
export function traceViewerStop() {
  return api('POST', '/api/trace-viewer/stop')
}
export function openTraceViewer() {
  return api<{ success?: boolean }>('POST', '/api/trace-viewer/open')
}

// ───────────────────────────────────────────────────────────────────────────
// 模拟(伪造)账号 plugin 模式(/api/desktop/fake-account/*,MOC-257)
// 无真实 ChatGPT 账号时的插件强制解锁:写合规伪造 auth.json(auth_mode=chatgpt + 合成 JWT)
// 让 Codex 原生显示 Plugins,proxy 截断 /backend-api/* 逐条伪造。替代不可靠的 CDP 注入档。
// ───────────────────────────────────────────────────────────────────────────
export interface FakeAccountStatus {
  /** 持久开关(用户意图);键缺失=null */
  modeEnabled: boolean | null
  /** 活动 auth.json 当前是否合成账号(伪造 relay 此刻是否真生效) */
  activeIsSynthetic: boolean
}

export async function getFakeAccountStatus(): Promise<FakeAccountStatus> {
  const r = await api<{ mode_enabled?: boolean | null; active_is_synthetic?: boolean }>(
    'GET',
    '/api/desktop/fake-account/status',
  )
  return { modeEnabled: r.mode_enabled ?? null, activeIsSynthetic: !!r.active_is_synthetic }
}

export function enableFakeAccount() {
  return api<{ success: boolean; message?: string }>('POST', '/api/desktop/fake-account/enable')
}

export function disableFakeAccount() {
  return api<{ success: boolean; message?: string }>('POST', '/api/desktop/fake-account/disable')
}
