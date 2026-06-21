import { computed, ref } from 'vue'
import {
  detectForeignSessions,
  importForeignSessions,
  restoreForeignSessions,
  type ForeignSession,
} from '@/api/desktop'
import { useConfirm } from './useConfirm'
import { useToast } from './useToast'
import { t, tFmt } from '@/i18n'

// [CAT-255] 导入/恢复其他工具留下的隔离会话(第三方 model_provider)。
//
// 机制:Codex 列表按当前活动 model_provider 过滤,transfer 锚点=openai。其他工具写的第三方
// tag 会话在 transfer 下隐藏。导入=第三方→openai(本工具可见);恢复=openai→记录的第三方值
// (其他工具可见,导入的逆操作)。两者都要后端关 Codex 写 state DB 再重启。
//
// `scanned` 记录**上次非空扫描**结果:导入后再扫会返回空(都变 openai 了),不能把记录冲掉,
// 所以只在扫到非空时覆盖 —— 这样导入后「恢复」下拉框仍有原 provider 可选(= 用户要的「前端记录」)。
const scanned = ref<ForeignSession[]>([])
const busy = ref(false)

export function useSessionImport() {
  const { confirm } = useConfirm()
  const { show: toast } = useToast()

  /** 扫描第三方会话,返回当前 count;非空结果记进 `scanned`(供恢复下拉用)。 */
  async function detect(): Promise<number> {
    const r = await detectForeignSessions().catch(() => null)
    if (r && r.sessions.length > 0) scanned.value = r.sessions
    return r?.count ?? 0
  }

  /** 扫描记录到的不同 model_provider(恢复下拉框选项)。 */
  const providers = computed(() => [...new Set(scanned.value.map((s) => s.modelProvider))])

  /** 导入:确认弹窗[关闭Codex/取消] → 全部第三方归一 openai → toast。 */
  async function promptImport(count: number) {
    const ok = await confirm({
      title: tFmt('settings.sessionImportTitle', { count }),
      message: t('settings.sessionImportMessage'),
      confirmLabel: t('settings.sessionImportConfirmBtn'), // 关闭 Codex
      cancelLabel: t('common.cancel'),
    })
    if (!ok) return
    busy.value = true
    try {
      const r = await importForeignSessions()
      toast(tFmt('settings.sessionImportOk', { count: r.imported }))
    } catch (e) {
      toast((e as Error).message || t('settings.sessionImportFailed'), 'error')
    } finally {
      busy.value = false
    }
  }

  /** 设置页「导入」按钮:先扫,无可导入则提示,有则走确认弹窗。 */
  async function importFromSettings() {
    const count = await detect()
    if (count === 0) {
      toast(t('settings.sessionImportEmpty'))
      return
    }
    await promptImport(count)
  }

  /** 恢复:把 `scanned` 里属于 `provider` 的会话 model_provider 写回该值 → toast。 */
  async function restore(provider: string) {
    const ids = scanned.value.filter((s) => s.modelProvider === provider).map((s) => s.id)
    if (ids.length === 0) {
      toast(t('settings.sessionRestoreEmpty'))
      return
    }
    busy.value = true
    try {
      const r = await restoreForeignSessions(ids, provider)
      toast(tFmt('settings.sessionRestoreOk', { count: r.imported, provider }))
    } catch (e) {
      toast((e as Error).message || t('settings.sessionRestoreFailed'), 'error')
    } finally {
      busy.value = false
    }
  }

  return { scanned, providers, busy, detect, promptImport, importFromSettings, restore }
}
