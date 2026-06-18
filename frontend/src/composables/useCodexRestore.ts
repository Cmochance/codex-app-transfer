// 共享「还原 Codex 原配置」流程(DesktopPage 的 clear-desktop + Settings 快照面板复用)。
// 逐字移植旧 app.js chooseCodexRestoreTarget / formatCodexSnapshotChoice + clear-desktop action。
// window.confirm / window.prompt 在 webview 可用,保留旧交互(多快照选序号)。
import { t, tFmt } from '@/i18n'
import { useToast } from './useToast'
import {
  getDesktopSnapshots,
  restoreDesktopSnapshot,
  clearDesktop,
  type SnapshotInfo,
} from '@/api/desktop'

function formatChoice(s: SnapshotInfo, index: number): string {
  const kind = t(`settings.codexSnapshotKind.${s.kind || 'unknown'}`)
  const provider = s.providerName || t('settings.codexSnapshotProviderUnknown')
  const time = s.snapshotAt || t('settings.codexSnapshotTimeUnknown')
  const version = s.appVersion || t('settings.codexSnapshotVersionUnknown')
  const files =
    [s.configExisted ? 'config.toml' : null, s.authExisted ? 'auth.json' : null]
      .filter(Boolean)
      .join(' + ') || t('settings.codexSnapshotFilesNone')
  return `${index + 1}. ${time} | ${kind} | ${provider} | ${version} | ${files}`
}

type RestoreTarget = { snapshotId?: string; fallback?: boolean } | null

export function useCodexRestore() {
  const { show: toast } = useToast()

  // 选择还原目标:0 快照→legacy fallback 确认;1→单确认;>1→prompt 选序号。
  async function chooseTarget(): Promise<RestoreTarget> {
    const snapshots = await getDesktopSnapshots()
    if (!snapshots.length) {
      return window.confirm(t('confirm.desktopClearFallback')) ? { fallback: true } : null
    }
    if (snapshots.length === 1) {
      const summary = formatChoice(snapshots[0], 0)
      return window.confirm(tFmt('confirm.desktopSnapshotRestoreSingle', { summary }))
        ? { snapshotId: snapshots[0].id }
        : null
    }
    const list = snapshots.map(formatChoice).join('\n')
    const input = window.prompt(tFmt('confirm.desktopSnapshotSelect', { list }))
    if (input === null) return null
    const selectedIndex = Number.parseInt(String(input).trim(), 10) - 1
    if (!Number.isInteger(selectedIndex) || selectedIndex < 0 || selectedIndex >= snapshots.length) {
      toast(t('toast.desktopSnapshotInvalid'))
      return null
    }
    const summary = formatChoice(snapshots[selectedIndex], selectedIndex)
    if (!window.confirm(tFmt('confirm.desktopSnapshotRestoreSelected', { summary }))) return null
    return { snapshotId: snapshots[selectedIndex].id }
  }

  // 执行还原。返回 true=已执行(调用方刷新状态)/ false=用户取消。
  // API 错误抛给调用方(在外层 try/catch toast),与旧 handleAction 一致。
  async function restoreCodexConfig(): Promise<boolean> {
    const target = await chooseTarget()
    if (!target) return false
    const result = target.snapshotId
      ? await restoreDesktopSnapshot(target.snapshotId)
      : await clearDesktop()
    const fellBackToLegacy = result?.restored === false
    toast(t(fellBackToLegacy ? 'toast.desktopClearedLegacy' : 'toast.desktopCleared'))
    return true
  }

  return { restoreCodexConfig }
}
