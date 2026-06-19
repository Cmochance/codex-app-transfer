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
// 背景全图 on-demand 下载进度(apply 期间轮询,渲染缩略图进度环 + 白蒙版)。
// downloading:false = 已缓存 / 未触发 / 下载结束。
export interface ThemeBgProgress {
  downloading: boolean
  downloaded?: number
  total?: number
}
export function themeBgProgress(themeId: string) {
  return api<ThemeBgProgress>(
    'GET',
    `/api/desktop/theme/bg-progress?theme_id=${encodeURIComponent(themeId)}`,
  )
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

// ───────────────────────────────────────────────────────────────────────────
// 三态插件解锁选择器(/api/desktop/plugin-unlock/*,MOC-257)
// 统一「关闭 / 模拟账号 / 真实账号」三态,取代旧的 autoUnlockCodexPlugins(CDP,废弃)+ 模拟账号开关。
// synthetic:写合成 auth.json + proxy 伪造;real:用真实账号 relay 透传;off:转移备份 auth.json、退出还原。
// ───────────────────────────────────────────────────────────────────────────
export type PluginUnlockMode = 'off' | 'synthetic' | 'real'

export interface PluginUnlockStatus {
  /** 最近成功 apply 的**实际生效**三态;**null = 未 apply 过**(启动跳过 / 首次,~/.codex 是 restore 后原态) */
  mode: PluginUnlockMode | null
  /** 持久值(用户是否手动设过);null = 未设、走默认推导 */
  persisted: PluginUnlockMode | null
  /** 本地是否有真账号(活动或 stash,含失效的) */
  hasRealAccount: boolean
  /** 真账号是否**实际可用**(非空 + 未过期 + 未撤销) */
  realAccountUsable: boolean
  /** 活动 auth.json 当前是否合成账号 */
  activeIsSynthetic: boolean
}

export async function getPluginUnlockStatus(): Promise<PluginUnlockStatus> {
  const r = await api<{
    mode?: PluginUnlockMode | null
    persisted?: PluginUnlockMode | null
    hasRealAccount?: boolean
    realAccountUsable?: boolean
    activeIsSynthetic?: boolean
  }>('GET', '/api/desktop/plugin-unlock/status')
  return {
    mode: r.mode ?? null,
    persisted: r.persisted ?? null,
    hasRealAccount: !!r.hasRealAccount,
    realAccountUsable: !!r.realAccountUsable,
    activeIsSynthetic: !!r.activeIsSynthetic,
  }
}

export interface SetPluginUnlockResult {
  success: boolean
  /** 用户意图(持久) */
  mode: PluginUnlockMode
  /** 实际生效(real 失效会降级 synthetic) */
  effective?: PluginUnlockMode
  /** real 是否被降级成 synthetic */
  degraded?: boolean
  message?: string
}

export function setPluginUnlockMode(mode: PluginUnlockMode) {
  return api<SetPluginUnlockResult>('POST', '/api/desktop/plugin-unlock/set', { mode })
}

// ── 真实账号登录(codex login,/api/desktop/real-account/*)——「真实账号」档无账号时引导登录 ──
export type RealAccountLoginState = 'idle' | 'running' | 'succeeded' | 'failed' | 'cancelled'

export interface RealAccountLoginStatus {
  loggedIn: boolean
  loginState: RealAccountLoginState
  loginMessage?: string
}

/** 读真账号 + 登录流程状态(login 字段是 serde tagged {state, message})。 */
export async function getRealAccountStatus(): Promise<RealAccountLoginStatus> {
  const r = await api<{
    status?: { logged_in?: boolean }
    login?: { state?: RealAccountLoginState; message?: string } | RealAccountLoginState
  }>('GET', '/api/desktop/real-account/status')
  const login = r.login ?? {}
  const loginState = (typeof login === 'string' ? login : login.state) ?? 'idle'
  const loginMessage = typeof login === 'string' ? undefined : login.message
  return { loggedIn: !!r.status?.logged_in, loginState, loginMessage }
}

/** 启动官方 codex login(非阻塞,会弹浏览器做 ChatGPT OAuth)。 */
export function startRealAccountLogin() {
  return api<{ success: boolean; message?: string }>('POST', '/api/desktop/real-account/login')
}

export function cancelRealAccountLogin() {
  return api<{ success: boolean; cancelled?: boolean }>(
    'POST',
    '/api/desktop/real-account/login/cancel',
  )
}
