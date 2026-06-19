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
// 在系统文件管理器打开 Codex 原配置快照目录(~/.codex-app-transfer/codex-snapshots/active/)
export function openSnapshotDir() {
  return api('POST', '/api/desktop/open-snapshot-dir')
}

// ───────────────────────────────────────────────────────────────────────────
// Codex 插件解锁 daemon(CDP 注入,/api/desktop/plugin-unlock/*)
// ───────────────────────────────────────────────────────────────────────────
export type PluginUnlockState =
  | 'disconnected'
  | 'connecting'
  | 'connected'
  | 'injected'
  | 'failed'
export interface PluginUnlockStatusResp {
  status: PluginUnlockState
  message: string
}
export function pluginUnlockStatus() {
  return api<PluginUnlockStatusResp>('GET', '/api/desktop/plugin-unlock/status')
}
export function pluginUnlockStart() {
  return api('POST', '/api/desktop/plugin-unlock/start')
}
export function pluginUnlockReinject() {
  return api('POST', '/api/desktop/plugin-unlock/reinject')
}

// ───────────────────────────────────────────────────────────────────────────
// 还原 Codex 原配置(/api/desktop/clear,设置页快照「还原」复用 useCodexRestore)
// ───────────────────────────────────────────────────────────────────────────
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
