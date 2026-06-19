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
// 三态插件解锁选择器(/api/desktop/plugin-unlock/*,MOC-257)
// 统一「关闭 / 模拟账号 / 真实账号」三态,取代旧的 autoUnlockCodexPlugins(CDP,废弃)+ 模拟账号开关。
// synthetic:写合成 auth.json + proxy 伪造;real:用真实账号 relay 透传;off:转移备份 auth.json、退出还原。
// ───────────────────────────────────────────────────────────────────────────
export type PluginUnlockMode = 'off' | 'synthetic' | 'real'

export interface PluginUnlockStatus {
  /** 当前**生效**三态(持久值优先,缺失按真账号推导) */
  mode: PluginUnlockMode
  /** 持久值(用户是否手动设过);null = 未设、走默认推导 */
  persisted: PluginUnlockMode | null
  /** 本地是否有真实 chatgpt 账号可用(活动或 stash) */
  hasRealAccount: boolean
  /** 活动 auth.json 当前是否合成账号 */
  activeIsSynthetic: boolean
}

export async function getPluginUnlockStatus(): Promise<PluginUnlockStatus> {
  const r = await api<{
    mode?: PluginUnlockMode
    persisted?: PluginUnlockMode | null
    hasRealAccount?: boolean
    activeIsSynthetic?: boolean
  }>('GET', '/api/desktop/plugin-unlock/status')
  return {
    mode: r.mode ?? 'synthetic',
    persisted: r.persisted ?? null,
    hasRealAccount: !!r.hasRealAccount,
    activeIsSynthetic: !!r.activeIsSynthetic,
  }
}

export function setPluginUnlockMode(mode: PluginUnlockMode) {
  return api<{ success: boolean; message?: string }>('POST', '/api/desktop/plugin-unlock/set', {
    mode,
  })
}
