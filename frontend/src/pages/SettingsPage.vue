<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { i18nState, setLocale, t, tFmt } from '@/i18n'
import { useAppearance, type Appearance } from '@/composables/useAppearance'
import { useFont, type FontChoice, type FontSize } from '@/composables/useFont'
import { useSettingsStore } from '@/stores/settings'
import { exportConfig, importConfig, type Settings } from '@/api/settings'
import { useToast } from '@/composables/useToast'
import { useConfirm } from '@/composables/useConfirm'
import { useSessionImport } from '@/composables/useSessionImport'
import { clearAllConversations } from '@/api/codex'
import { useMcpRecovery } from '@/composables/useMcpRecovery'
import { getAppVersion, checkAppUpdate, installAppUpdate, openExternalUrl } from '@/api/system'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'
import AppSelect from '@/components/ui/AppSelect.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppModal from '@/components/ui/AppModal.vue'
import { getChromeReady, ensureChrome, getSystemProxyStatus, type SystemProxyStatus } from '@/api/chrome'
import {
  getPluginUnlockStatus,
  setPluginUnlockMode,
  getRealAccountStatus,
  startRealAccountLogin,
  cancelRealAccountLogin,
  pinCurrentRealAccount,
  getSuperpowersStatus,
  reconcileSuperpowers,
  type PluginUnlockMode,
  type SuperpowersStatus,
} from '@/api/desktop'
import type { ApiError } from '@/api/http'
import ResidualScanPanel from '@/components/settings/ResidualScanPanel.vue'
import SnapshotPanel from '@/components/settings/SnapshotPanel.vue'
import DiagnosticPanel from '@/components/settings/DiagnosticPanel.vue'
import FeedbackModal from '@/components/settings/FeedbackModal.vue'
import IconChevronRight from '~icons/lucide/chevron-right'

const store = useSettingsStore()
const { current: appearance, set: setAppearance } = useAppearance()
const { show: toast } = useToast()
const appVersion = ref('')
const feedbackOpen = ref(false)
const updateAvailable = ref(false)
const installSupported = ref(false)
const latestVersion = ref('')
const installModalOpen = ref(false)
const installing = ref(false)
// MOC-261 一-4:MCP 凭据丢失恢复 —— 入口状态(弹窗在 App.vue 全局渲染,此处仅展示状态 + 打开)。
const mcpRecovery = useMcpRecovery()
const mcpRecoveryStatus = computed(() =>
  mcpRecovery.pending.value > 0
    ? tFmt('settings.mcpRecoveryPending', { count: mcpRecovery.pending.value })
    : tFmt('settings.mcpRecoveryAllIgnored', { count: mcpRecovery.entries.value.length }),
)

// MOC-261 二-4:清空会话历史(两者都清)—— 全部 Codex 对话 rollout 移回收站(可恢复)+ 清 proxy 续轮缓存。
const { confirm } = useConfirm()
const clearingSessions = ref(false)
async function onClearSessionHistory() {
  const ok = await confirm({
    title: t('settings.sessionHistoryClearConfirmTitle'),
    message: t('settings.sessionHistoryClearConfirm'),
    confirmLabel: t('settings.sessionHistoryClear'),
    danger: true,
  })
  if (!ok) return
  clearingSessions.value = true
  try {
    const r = await clearAllConversations()
    if (r.sessionsFailed > 0) {
      // 部分 / 全部 rollout 移回收站失败(权限/沙箱等)→ 明确告知,别报成功(隐私清除未达成)。
      toast(
        tFmt('settings.sessionHistoryClearPartial', {
          ok: r.sessionsTrashed,
          failed: r.sessionsFailed,
        }),
        'error',
      )
    } else {
      toast(tFmt('settings.sessionHistoryClearOk', { count: r.sessionsTrashed }))
    }
  } catch (e) {
    toast((e as Error).message || t('settings.sessionHistoryClearFailed'), 'error')
  } finally {
    clearingSessions.value = false
  }
}

// CAT-255:导入/恢复其他工具留下的隔离会话(第三方 model_provider)。
const sessionImport = useSessionImport()
const restoreOpen = ref(false)
const restoreProvider = ref('')
// 「导入」按钮:扫第三方 → 有则确认弹窗[关闭Codex]导入成 openai
function onImportSessions() {
  void sessionImport.importFromSettings()
}
// 「恢复」按钮:先扫(刷新记录)→ 有记录的 provider 才开下拉弹窗
async function onRestoreSessions() {
  await sessionImport.detect()
  if (sessionImport.providers.value.length === 0) {
    toast(t('settings.sessionRestoreEmpty'))
    return
  }
  restoreProvider.value = sessionImport.providers.value[0]
  restoreOpen.value = true
}
// 弹窗里选好 provider、点「关闭Codex」→ 写回 → 关弹窗
async function onRestoreConfirm() {
  await sessionImport.restore(restoreProvider.value)
  restoreOpen.value = false
}

onMounted(() => {
  if (!store.loaded) store.load().catch(() => {})
  getAppVersion()
    .then((r) => (appVersion.value = r.version || ''))
    .catch(() => {})
  refreshPluginUnlockStatus()
  refreshSuperpowersStatus()
  mcpRecovery.refresh()
})

// 拉后端三态状态同步两 ref(mount + 「还原 Codex 原配置」后,后端 reset 了 LAST_APPLIED 需重读)。
function refreshPluginUnlockStatus() {
  getPluginUnlockStatus()
    .then((s) => {
      pluginUnlockMode.value = s.mode // 显示:实际生效(未 apply→null)
      persistedMode.value = s.persisted // 意图:持久值(降级时仍是 real)
    })
    .catch(() => {})
}

// 关于:检查更新 + 外链(走系统浏览器)
async function onCheckUpdate() {
  try {
    // 以后端权威 updateAvailable 为准(不再用 latest!=appVersion 字符串比较兜底,避免 /api/version
    // 未就绪 / 格式差异误判);installSupported=false(如 Linux)时不显安装按钮,仅提示有更新。
    const r = await checkAppUpdate()
    if (r.updateAvailable) {
      updateAvailable.value = true
      installSupported.value = !!r.installSupported
      latestVersion.value = r.latestVersion || ''
      toast(tFmt('about.updateAvailable', { version: r.latestVersion || '' }))
    } else {
      updateAvailable.value = false
      installSupported.value = false
      toast(t('about.upToDate'))
    }
  } catch (e) {
    toast((e as Error).message || t('about.checkFailed'), 'error')
  }
}
// 「能查不能装」补齐:有更新时弹确认 → 调 /api/update/install(下载 installer + app 退出拉起)。
async function confirmInstall() {
  if (installing.value) return
  installing.value = true
  try {
    const r = await installAppUpdate()
    // installer 已启动、app 即将退出;尽量先把后端提示 toast 出来(best-effort)。
    toast(r.message || t('settings.installUpdate'))
    installModalOpen.value = false
  } catch (e) {
    // 后端错误(如 macOS translocation:未拖进 /Applications)原样透出指引。
    toast((e as Error).message || t('about.checkFailed'), 'error')
  } finally {
    installing.value = false
  }
}
function openExternal(url: string) {
  openExternalUrl(url).catch((e) => toast((e as Error).message, 'error'))
}

// 配置文件:导出 → 下载整份配置 JSON(含 API key);导入 → 选文件读 JSON → POST
// (后端自动 before-import 备份 + 保留现有 provider secret + normalize 校验)。
function downloadJson(obj: unknown, filename: string) {
  const blob = new Blob([JSON.stringify(obj, null, 2)], { type: 'application/json' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = filename
  a.click()
  URL.revokeObjectURL(url)
}
async function onExportConfig() {
  try {
    const data = await exportConfig()
    const stamp = new Date().toISOString().slice(0, 10)
    downloadJson(data, `codex-app-transfer-config-${stamp}.json`)
    toast(t('settings.exportConfigOk'))
  } catch (e) {
    toast((e as Error).message, 'error')
  }
}
function onImportConfig() {
  const input = document.createElement('input')
  input.type = 'file'
  input.accept = 'application/json,.json'
  input.style.display = 'none'
  document.body.appendChild(input)
  input.addEventListener('change', async () => {
    const file = input.files?.[0]
    input.remove()
    if (!file) return
    try {
      const parsed = JSON.parse(await file.text())
      await importConfig(parsed)
      // import 替换整 config:刷新 store,并**立即重应用主题/语言** —— 它们由 useAppearance 单例 +
      // i18nState 驱动、只在启动从 settings 注水(见 App.vue),不在此重应用则导入改了 theme/language
      // 要重启才生效(段控件 + 实际外观停在旧值)。这里镜像 App.vue 的注水逻辑。
      const reloaded = await store.load()
      if (typeof reloaded.theme === 'string') useAppearance().load(reloaded.theme)
      if (reloaded.language === 'zh' || reloaded.language === 'en')
        setLocale(reloaded.language as 'zh' | 'en')
      // 插件解锁档(settings.pluginUnlockMode)同由独立 ref 驱动(refreshPluginUnlockStatus),
      // 刷新使段控件 + no-op 守卫反映导入后的持久档。(font 走 localStorage、不在 settings,import 不动它。)
      refreshPluginUnlockStatus()
      toast(t('settings.importConfigOk'))
    } catch (e) {
      toast((e as Error).message, 'error')
    }
  })
  input.click()
}

// 保存 partial → 后端浅合并;store.save 已做乐观更新 + 失败回滚,这里只 toast。
async function persist(partial: Settings) {
  try {
    const warn = await store.save(partial)
    if (warn) toast(warn, 'error')
  } catch (e) {
    toast((e as Error).message || t('theme.saveFailed'), 'error')
  }
}

// boolean 开关 writable computed(默认值复刻旧 renderSettings 的 !==false / ===true 语义)
function toggle(key: string, def: boolean) {
  return computed<boolean>({
    get: () => store.bool(key, def),
    set: (v) => void persist({ [key]: v }),
  })
}
const autoApplyOnStart = toggle('autoApplyOnStart', true)
const restoreCodexOnExit = toggle('restoreCodexOnExit', true)
const autoWakeCodexPet = toggle('autoWakeCodexPet', true)
const codexQuotaEnabled = toggle('codexQuotaEnabled', false)
const codexStashEnabled = toggle('codexStashEnabled', false)
const codexNetworkAccess = toggle('codexNetworkAccess', false)
const exposeAllProviderModels = toggle('exposeAllProviderModels', false)
const showGrayProviders = toggle('showGrayProviders', false)
const mcpCredentialsPortableStore = toggle('mcpCredentialsPortableStore', true)
const hideDockIcon = toggle('hideDockIcon', false)
// [MOC-277] superpowers 强约束插件开关:默认态由后端算(已自装 superpowers → 默认关,避免双装),
// 翻开关后即时收敛(装/卸插件文件),否则要等下次 apply / Codex relaunch 才生效。
const superpowersStatus = ref<SuperpowersStatus | null>(null)
const antigravitySuperpowers = computed<boolean>({
  get: () =>
    store.bool('antigravitySuperpowersEnabled', superpowersStatus.value?.effectiveEnabled ?? true),
  set: (v) => {
    void (async () => {
      await persist({ antigravitySuperpowersEnabled: v })
      try {
        superpowersStatus.value = await reconcileSuperpowers()
      } catch {
        /* best-effort:收敛失败不影响设置已存,下次 apply 仍会生效 */
      }
    })()
  },
})
function refreshSuperpowersStatus() {
  getSuperpowersStatus()
    .then((s) => (superpowersStatus.value = s))
    .catch(() => {})
}
// macOS 限定:隐藏程序坞图标(Windows/Linux 无 Dock 概念,该开关不显示)
const isMac = typeof navigator !== 'undefined' && /Mac/i.test(navigator.userAgent)

// [MOC-257 三态] 插件解锁三态(关闭/模拟账号/真实账号):非普通 settings 键,调专用 set 端点
// (写/移走 auth.json + apply relay + 驱动 proxy 伪造)。受控:高亮跟服务端,失败回滚 + toast。
// null = 后端未 apply 过(启动跳过 / 首次)→ SegmentedControl 不高亮,点任一档都触发 apply(否则
// 高亮某档时再点它 no-op、永远应用不上,见 review)。
const pluginUnlockMode = ref<PluginUnlockMode | null>(null)
// [review] 持久**意图**(降级时 displayed=synthetic 但 persisted 仍 real);no-op 判它而非 displayed,
// 否则降级后点 synthetic(displayed 已是 synthetic)会 no-op、settle 不到 synthetic、账号恢复又被升回 real。
const persistedMode = ref<PluginUnlockMode | null>(null)
const pluginUnlockBusy = ref(false)
const pluginUnlockOptions: { value: PluginUnlockMode; label: string }[] = [
  { value: 'off', label: t('settings.pluginUnlockOff') },
  { value: 'synthetic', label: t('settings.pluginUnlockSynthetic') },
  { value: 'real', label: t('settings.pluginUnlockReal') },
]
async function onSetPluginUnlockMode(mode: PluginUnlockMode) {
  // no-op 只在「持久意图 **且** 当前已生效」都等于点击档时(真没东西可改);否则放行 apply:
  // ① 降级(persisted=real / displayed=synthetic):点 synthetic 改意图 settle、点 real 重试;
  // ② 启动跳过 apply(persisted=某档 / displayed=null 未生效):点该档才能真应用上。
  if (
    pluginUnlockBusy.value ||
    (mode === persistedMode.value && mode === pluginUnlockMode.value)
  )
    return
  pluginUnlockBusy.value = true
  const prevDisplay = pluginUnlockMode.value
  const prevPersisted = persistedMode.value
  pluginUnlockMode.value = mode // 乐观更新(显示)
  persistedMode.value = mode // 乐观更新(意图,后端 persist 的就是 req.mode)
  try {
    const r = await setPluginUnlockMode(mode)
    if (r?.degraded && r.effective) {
      // 真账号失效 → 显示跟生效(合成);persistedMode 保持 real(意图),账号恢复可用会自动升回
      pluginUnlockMode.value = r.effective
      if (r.message) toast(r.message, 'error')
    } else if (r?.message) {
      toast(r.message)
    }
  } catch (e) {
    pluginUnlockMode.value = prevDisplay // 失败回滚
    persistedMode.value = prevPersisted
    const data = (e as ApiError)?.responseData as { needsLogin?: boolean } | undefined
    if (data?.needsLogin) {
      loginModalOpen.value = true // real 但本地无账号 → 弹登录提示
    } else {
      toast((e as Error).message || t('settings.pluginUnlockFailed'), 'error')
    }
  } finally {
    pluginUnlockBusy.value = false
  }
}

// [MOC-257] 真账号登录弹窗:选「真实账号」但本地无账号时引导用户 codex login。
const loginModalOpen = ref(false)
const loginRunning = ref(false)
const loginError = ref('')
let loginPollTimer: number | undefined
function stopLoginPoll() {
  if (loginPollTimer) {
    window.clearInterval(loginPollTimer)
    loginPollTimer = undefined
  }
}
async function startLogin() {
  loginError.value = ''
  loginRunning.value = true
  try {
    await startRealAccountLogin()
  } catch (e) {
    loginRunning.value = false
    loginError.value = (e as Error).message
    return
  }
  loginPollTimer = window.setInterval(async () => {
    try {
      const s = await getRealAccountStatus()
      if (s.loginState === 'succeeded') {
        stopLoginPoll()
        // [MOC-257 review] 切 real 前先 pin 当前账号到 mirror/stash:否则登录前已有快照(startup auto-apply)
        // + restoreCodexOnExit 开时,新登录账号没存进 mirror、退出 restore 重放登录前快照抹掉 auth_mode。
        // pin **失败则 block 切 real**:账号没进 mirror,Real 切了也撑不过退出 restore → 留 modal 显示错误、让
        // 用户修 ~/.codex-app-transfer 权限后重试,不带不可靠状态切 Real。
        try {
          await pinCurrentRealAccount()
        } catch {
          loginRunning.value = false
          loginError.value = t('settings.realAccountPinFailed')
          return // 保持 modal 打开显示错误,不切 real
        }
        loginRunning.value = false
        loginModalOpen.value = false
        toast(t('settings.realAccountLoginOk'))
        await onSetPluginUnlockMode('real') // 现在有账号了 + 已 pin,切真实账号
      } else if (s.loginState === 'failed') {
        stopLoginPoll()
        loginRunning.value = false
        loginError.value = s.loginMessage || t('settings.realAccountLoginFailed')
      } else if (s.loginState === 'cancelled' || s.loginState === 'idle') {
        stopLoginPoll()
        loginRunning.value = false
      }
    } catch {
      /* 轮询失败保持等待 */
    }
  }, 2000)
}
async function cancelLogin() {
  stopLoginPoll()
  loginRunning.value = false
  try {
    await cancelRealAccountLogin()
  } catch {
    /* ignore */
  }
}
function closeLoginModal() {
  if (loginRunning.value) cancelLogin()
  stopLoginPoll()
  loginModalOpen.value = false
}
onUnmounted(stopLoginPoll)

// theme / language 双向(同步本地状态 + 持久化服务端)。
// setAppearance/setLocale 立刻改 DOM/localStorage(无闪烁),但服务端保存失败时
// store.save 只回滚 Pinia settings、不动这二者 → UI 会停在未保存值。故失败时显式回滚。
const theme = computed<Appearance>({
  get: () => appearance.value,
  set: (v) => {
    const prev = appearance.value
    setAppearance(v)
    store
      .save({ theme: v })
      .then((warn) => warn && toast(warn, 'error'))
      .catch((e) => {
        // 仅当当前显示仍是本次所设值才回滚,避免快速连点时覆盖更晚成功的切换
        if (appearance.value === v) setAppearance(prev)
        toast((e as Error).message || t('theme.saveFailed'), 'error')
      })
  },
})
const language = computed<'zh' | 'en'>({
  get: () => i18nState.locale,
  set: (v) => {
    const prev = i18nState.locale
    setLocale(v)
    store
      .save({ language: v })
      .then((warn) => warn && toast(warn, 'error'))
      .catch((e) => {
        if (i18nState.locale === v) setLocale(prev)
        toast((e as Error).message || t('theme.saveFailed'), 'error')
      })
  },
})
const themeOptions: { value: Appearance; label: string }[] = [
  { value: 'light', label: t('settings.themeLight') },
  { value: 'dark', label: t('settings.themeDark') },
  { value: 'inkwash', label: t('settings.themeInkwash') },
]
const langOptions: { value: 'zh' | 'en'; label: string }[] = [
  { value: 'zh', label: '中文' },
  { value: 'en', label: 'EN' },
]

// 字体:按角色(正文/标题/等宽)+ 字号,纯 localStorage(useFont)。默认值 = 米原字体。
const font = useFont()
const bodyFont = computed<FontChoice>({ get: () => font.body.value, set: (v) => font.setRole('body', v) })
const headingFont = computed<FontChoice>({
  get: () => font.heading.value,
  set: (v) => font.setRole('heading', v),
})
const monoFont = computed<FontChoice>({ get: () => font.mono.value, set: (v) => font.setRole('mono', v) })
const fontSize = computed<FontSize>({ get: () => font.size.value, set: (v) => font.setSize(v) })
const bodyFontOptions: { value: FontChoice; label: string }[] = [
  { value: 'system', label: t('settings.fontSystem') },
  { value: 'songti', label: t('settings.fontSongti') },
  { value: 'kaiti', label: t('settings.fontKaiti') },
  { value: 'rounded', label: t('settings.fontRounded') },
]
const headingFontOptions: { value: FontChoice; label: string }[] = [
  { value: 'songti', label: t('settings.fontSongti') },
  { value: 'kaiti', label: t('settings.fontKaiti') },
  { value: 'system', label: t('settings.fontSystem') },
]
const monoFontOptions: { value: FontChoice; label: string }[] = [
  { value: 'mono', label: t('settings.fontMonoLabel') },
  { value: 'songti', label: t('settings.fontSongti') },
  { value: 'system', label: t('settings.fontSystem') },
]
const fontSizeOptions: { value: FontSize; label: string }[] = [
  { value: 'small', label: t('settings.fontSizeSmall') },
  { value: 'normal', label: t('settings.fontSizeNormal') },
  { value: 'large', label: t('settings.fontSizeLarge') },
]

// webFetchBackend(off/auto/curl/wreq/headless;仅 off/auto 有 i18n,余技术名)
// MOC-256:auto/headless 需真浏览器(Chrome)+ 系统代理就绪 → persist 前门控
//(系统代理 gate → detect → 无 Chrome 则弹确认 modal → ensure 按需下载),其余档直存。
const wfbDisplay = ref(store.str('webFetchBackend', 'auto'))
const wfbSwitching = ref(false) // in-flight guard,防 ~20s 下载期间重复点
const wfbPending = ref<string | null>(null) // 下载确认后要启用的档
const wfbDownloadModal = ref(false)
// store 变更(load / 外部)同步显示值,但门控进行中不打断用户当前选择
watch(
  () => store.str('webFetchBackend', 'auto'),
  (v) => {
    if (!wfbSwitching.value) wfbDisplay.value = v
  },
)
const webFetchOptions: { value: string; label: string }[] = [
  { value: 'off', label: t('settings.webFetchBackend.off') },
  { value: 'auto', label: t('settings.webFetchBackend.auto') },
  { value: 'curl', label: 'curl' },
  { value: 'wreq', label: 'wreq' },
  { value: 'headless', label: 'headless' },
]

// 存档某档:乐观更新 wfbDisplay + store.save。webFetchSyncWarning(注册到 Codex 失败)
// 不回退、仅警告并返 false 跳成功 toast;真保存失败回退到上次值 + 报「设置保存失败」(区分「下载失败」)。
async function commitWebFetch(v: string): Promise<boolean> {
  const prev = store.str('webFetchBackend', 'auto') // 失败回退目标,不依赖 store.save 内部回滚时序
  wfbDisplay.value = v
  try {
    const warn = await store.save({ webFetchBackend: v })
    if (warn) {
      toast(warn, 'error')
      return false
    }
    return true
  } catch (e) {
    wfbDisplay.value = prev
    const msg = (e as Error).message
    toast(t('settings.webFetchSaveFailed') + (msg ? `: ${msg}` : ''), 'error')
    return false
  }
}

async function onWebFetchChange(v: string | undefined) {
  if (!v || wfbSwitching.value || v === store.str('webFetchBackend', 'auto')) return
  // off/curl/wreq 不需浏览器 → 直接存档
  if (v !== 'auto' && v !== 'headless') {
    await commitWebFetch(v)
    return
  }
  wfbSwitching.value = true
  wfbDisplay.value = v
  let pendingModal = false
  try {
    // 系统代理门槛(MOC-161):配了梯子但连不上 → 降级 wreq;没配 / PAC / 查询失败一律 fail-open。
    let sp: SystemProxyStatus | null = null
    try {
      sp = (await getSystemProxyStatus()).systemProxy ?? null
    } catch (e) {
      // fail-open(查询失败放行),但留痕便于真机 DevTools 定位后端回归
      console.warn('[webFetch gate] system-proxy status probe failed, fail-open:', e)
      sp = null
    }
    const gateOk = !sp || sp.kind === 'pac' || !sp.configured || sp.connected === true
    if (!gateOk) {
      toast(t('settings.webFetchAutoNeedsProxy'))
      await commitWebFetch('wreq')
      return
    }
    // Chrome readiness:就绪(系统 Chrome 自检过 / 已下载 shell)直接存,未就绪弹下载确认(modal 期间保持 guard)
    if ((await getChromeReady()).ready) {
      if (await commitWebFetch(v)) toast(t('settings.headlessChromeSystemFound'))
    } else {
      pendingModal = true
      wfbPending.value = v
      wfbDownloadModal.value = true
    }
  } catch (e) {
    wfbDisplay.value = store.str('webFetchBackend', 'auto')
    toast((e as Error).message || t('settings.headlessChromeFailed'), 'error')
  } finally {
    if (!pendingModal) wfbSwitching.value = false
  }
}

async function onChromeDownloadConfirm() {
  wfbDownloadModal.value = false
  toast(t('settings.headlessChromeDownloading'))
  try {
    await ensureChrome()
  } catch (e) {
    // 下载本身失败 → 回退高亮,报「下载失败」
    toast((e as Error).message || t('settings.headlessChromeFailed'), 'error')
    wfbDisplay.value = store.str('webFetchBackend', 'auto')
    wfbSwitching.value = false
    return
  }
  const bk = wfbPending.value || 'headless'
  if (await commitWebFetch(bk)) toast(t('settings.headlessChromeDownloaded'))
  wfbPending.value = null
  wfbSwitching.value = false
}

function onChromeDownloadCancel() {
  wfbDownloadModal.value = false
  wfbDisplay.value = store.str('webFetchBackend', 'auto') // 取消回退到上次保存值
  wfbPending.value = null
  wfbSwitching.value = false
}

function onPort(key: 'proxyPort' | 'adminPort', e: Event) {
  const v = Number((e.target as HTMLInputElement).value)
  if (Number.isFinite(v) && v > 0) void persist({ [key]: v })
}
// 更新地址写死本仓库(不可自定义);后端 DEFAULT_UPDATE_URL 同样指向它
const UPDATE_REPO_URL = 'https://github.com/Cmochance/codex-app-transfer'
</script>

<template>
  <div>
    <SettingsGroup :title="t('settings.groupAppearance')">
      <SettingsRow :title="t('settings.theme')" :description="t('settings.themeDesc')">
        <SegmentedControl v-model="theme" :options="themeOptions" />
      </SettingsRow>
      <SettingsRow :title="t('settings.language')" :description="t('settings.langDesc')">
        <SegmentedControl v-model="language" :options="langOptions" />
      </SettingsRow>
      <SettingsRow :title="t('settings.fontBody')" :description="t('settings.fontBodyDesc')">
        <AppSelect v-model="bodyFont" :options="bodyFontOptions" class="font-select" />
      </SettingsRow>
      <SettingsRow :title="t('settings.fontHeading')" :description="t('settings.fontHeadingDesc')">
        <AppSelect v-model="headingFont" :options="headingFontOptions" class="font-select" />
      </SettingsRow>
      <SettingsRow :title="t('settings.fontMono')" :description="t('settings.fontMonoDesc')">
        <AppSelect v-model="monoFont" :options="monoFontOptions" class="font-select" />
      </SettingsRow>
      <SettingsRow :title="t('settings.fontSize')" :description="t('settings.fontSizeDesc')">
        <SegmentedControl v-model="fontSize" :options="fontSizeOptions" />
      </SettingsRow>
      <SettingsRow
        v-if="isMac"
        :title="t('settings.hideDockIcon')"
        :description="t('settings.hideDockIconHint')"
      >
        <AppSwitch v-model="hideDockIcon" />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup :title="t('settings.groupStartup')">
      <SettingsRow :title="t('settings.autoApplyOnStart')" :description="t('settings.autoApplyOnStartHint')">
        <AppSwitch v-model="autoApplyOnStart" />
      </SettingsRow>
      <SettingsRow :title="t('settings.restoreCodexOnExit')" :description="t('settings.restoreCodexOnExitHint')">
        <AppSwitch v-model="restoreCodexOnExit" />
      </SettingsRow>
      <SettingsRow :title="t('settings.pluginUnlock')" :description="t('settings.pluginUnlockHint')">
        <SegmentedControl
          :model-value="pluginUnlockMode ?? undefined"
          :options="pluginUnlockOptions"
          @update:model-value="(m) => onSetPluginUnlockMode(m as PluginUnlockMode)"
        />
      </SettingsRow>
      <SettingsRow :title="t('settings.autoWakeCodexPet')" :description="t('settings.autoWakeCodexPetHint')">
        <AppSwitch v-model="autoWakeCodexPet" />
      </SettingsRow>
      <SettingsRow :title="t('settings.codexNetworkAccess')" :description="t('settings.codexNetworkAccessHint')">
        <AppSwitch v-model="codexNetworkAccess" />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup :title="t('settings.groupCodexIntegration')">
      <SettingsRow :title="t('settings.codexQuotaEnabled')" :description="t('settings.codexQuotaEnabledHint')">
        <AppSwitch v-model="codexQuotaEnabled" />
      </SettingsRow>
      <SettingsRow :title="t('settings.codexStashEnabled')" :description="t('settings.codexStashEnabledHint')">
        <AppSwitch v-model="codexStashEnabled" />
      </SettingsRow>
      <RouterLink to="/codex-skin" class="nav-row">
        <div class="nav-row__text">
          <div class="nav-row__title">{{ t('theme.title') }}</div>
          <div class="nav-row__desc">{{ t('settings.codexThemeRowDesc') }}</div>
        </div>
        <IconChevronRight class="nav-row__chevron" />
      </RouterLink>
      <SettingsRow :title="t('settings.webFetchBackend')" :description="t('settings.webFetchBackendHint')">
        <SegmentedControl
          :model-value="wfbDisplay"
          :options="webFetchOptions"
          @update:model-value="onWebFetchChange"
        />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup :title="t('settings.groupCodexConfig')">
      <ResidualScanPanel />
      <SnapshotPanel @restored="refreshPluginUnlockStatus" />
    </SettingsGroup>

    <SettingsGroup :title="t('settings.groupAdvanced')">
      <SettingsRow :title="t('settings.exposeAllModels')" :description="t('settings.exposeAllModelsDesc')">
        <AppSwitch v-model="exposeAllProviderModels" />
      </SettingsRow>
      <SettingsRow :title="t('settings.showGrayProviders')" :description="t('settings.showGrayProvidersHint')">
        <AppSwitch v-model="showGrayProviders" />
      </SettingsRow>
      <SettingsRow
        v-if="showGrayProviders"
        :title="t('settings.antigravitySuperpowers')"
        :description="t('settings.antigravitySuperpowersHint')"
      >
        <AppSwitch v-model="antigravitySuperpowers" />
      </SettingsRow>
      <SettingsRow
        :title="t('settings.mcpCredentialsPortableStore')"
        :description="t('settings.mcpCredentialsPortableStoreHint')"
      >
        <AppSwitch v-model="mcpCredentialsPortableStore" />
      </SettingsRow>
      <SettingsRow
        v-if="mcpRecovery.entries.value.length > 0"
        :title="t('settings.mcpRecoveryTitle')"
        :description="mcpRecoveryStatus"
      >
        <AppButton
          size="sm"
          variant="secondary"
          :label="t('settings.mcpRecoveryHandle')"
          @click="mcpRecovery.openModal()"
        />
      </SettingsRow>
      <SettingsRow
        :title="t('settings.sessionHistoryTitle')"
        :description="t('settings.sessionHistoryDesc')"
      >
        <AppButton
          size="sm"
          variant="secondary"
          :label="t('settings.sessionRestoreBtn')"
          :disabled="sessionImport.busy.value"
          @click="onRestoreSessions"
        />
        <AppButton
          size="sm"
          variant="secondary"
          :label="t('settings.sessionImportBtn')"
          :disabled="sessionImport.busy.value"
          @click="onImportSessions"
        />
        <AppButton
          size="sm"
          variant="danger"
          :label="t('settings.sessionClearBtn')"
          :disabled="clearingSessions"
          @click="onClearSessionHistory"
        />
      </SettingsRow>
      <SettingsRow :title="t('settings.proxyPort')" :description="t('settings.proxyPortDesc')">
        <input
          type="number"
          class="settings-num"
          :value="store.num('proxyPort', 0) || ''"
          min="1"
          max="65535"
          @change="onPort('proxyPort', $event)"
        />
      </SettingsRow>
      <SettingsRow :title="t('settings.adminPort')" :description="t('settings.adminPortDesc')">
        <input
          type="number"
          class="settings-num"
          :value="store.num('adminPort', 0) || ''"
          min="1"
          max="65535"
          @change="onPort('adminPort', $event)"
        />
      </SettingsRow>
      <DiagnosticPanel />
    </SettingsGroup>

    <SettingsGroup :title="t('about.group')">
      <SettingsRow :title="t('about.version')" :description="appVersion ? `v${appVersion}` : '…'">
        <AppButton size="sm" variant="secondary" :label="t('about.checkUpdate')" @click="onCheckUpdate" />
        <AppButton
          v-if="updateAvailable && installSupported"
          size="sm"
          variant="primary"
          :label="t('settings.installUpdate')"
          @click="installModalOpen = true"
        />
      </SettingsRow>
      <SettingsRow :title="t('settings.configBackup')" :description="t('settings.configBackupHint')">
        <AppButton size="sm" variant="secondary" :label="t('settings.exportConfig')" @click="onExportConfig" />
        <AppButton size="sm" variant="secondary" :label="t('settings.importConfig')" @click="onImportConfig" />
      </SettingsRow>
      <SettingsRow :title="t('settings.updateUrl')" :description="t('settings.updateUrlDesc')">
        <code class="settings-readonly">{{ UPDATE_REPO_URL }}</code>
      </SettingsRow>
      <SettingsRow :title="t('about.like')" :description="t('about.likeDesc')">
        <AppButton size="sm" variant="secondary" :label="t('about.like')" @click="openExternal(UPDATE_REPO_URL)" />
      </SettingsRow>
      <SettingsRow :title="t('about.feedback')" :description="t('about.feedbackDesc')">
        <AppButton size="sm" variant="secondary" :label="t('about.feedback')" @click="feedbackOpen = true" />
      </SettingsRow>
    </SettingsGroup>

    <FeedbackModal v-if="feedbackOpen" @close="feedbackOpen = false" />

    <AppModal
      v-if="wfbDownloadModal"
      :title="t('settings.headlessChromeTitle')"
      @close="onChromeDownloadCancel"
    >
      <div class="chrome-dl">
        <p class="chrome-dl__desc">{{ t('settings.headlessChromeDesc') }}</p>
        <div class="chrome-dl__actions">
          <AppButton variant="ghost" :label="t('common.cancel')" @click="onChromeDownloadCancel" />
          <AppButton
            variant="primary"
            :label="t('settings.headlessChromeConfirm')"
            @click="onChromeDownloadConfirm"
          />
        </div>
      </div>
    </AppModal>

    <AppModal
      v-if="loginModalOpen"
      :title="t('settings.realAccountLoginTitle')"
      @close="closeLoginModal"
    >
      <div class="chrome-dl">
        <p class="chrome-dl__desc">{{ t('settings.realAccountLoginPrompt') }}</p>
        <p v-if="loginRunning" class="chrome-dl__desc">
          {{ t('settings.realAccountLoginRunning') }}
        </p>
        <p v-if="loginError" class="chrome-dl__desc chrome-dl__err">{{ loginError }}</p>
        <div class="chrome-dl__actions">
          <AppButton variant="ghost" :label="t('common.cancel')" @click="closeLoginModal" />
          <AppButton
            v-if="!loginRunning"
            variant="primary"
            :label="t('settings.realAccountLoginGo')"
            @click="startLogin"
          />
          <AppButton
            v-else
            variant="ghost"
            :label="t('settings.realAccountLoginCancelBtn')"
            @click="cancelLogin"
          />
        </div>
      </div>
    </AppModal>

    <AppModal
      v-if="installModalOpen"
      :title="t('settings.installUpdate')"
      @close="installModalOpen = false"
    >
      <div class="chrome-dl">
        <p v-if="latestVersion" class="chrome-dl__desc">
          {{ tFmt('about.updateAvailable', { version: latestVersion }) }}
        </p>
        <p class="chrome-dl__desc">{{ t('confirm.installUpdate') }}</p>
        <div class="chrome-dl__actions">
          <AppButton variant="ghost" :label="t('common.cancel')" @click="installModalOpen = false" />
          <AppButton
            variant="primary"
            :label="t('settings.installUpdate')"
            :disabled="installing"
            @click="confirmInstall"
          />
        </div>
      </div>
    </AppModal>

    <!-- CAT-255:恢复会话到其他工具 —— 下拉选 model_provider,确认即关 Codex 写回再重启 -->
    <AppModal
      v-if="restoreOpen"
      :title="t('settings.sessionRestoreTitle')"
      @close="restoreOpen = false"
    >
      <div class="chrome-dl">
        <p class="chrome-dl__desc">{{ t('settings.sessionRestoreMessage') }}</p>
        <AppSelect
          v-model="restoreProvider"
          :options="sessionImport.providers.value.map((p) => ({ value: p, label: p }))"
        />
        <div class="chrome-dl__actions">
          <AppButton variant="ghost" :label="t('common.cancel')" @click="restoreOpen = false" />
          <AppButton
            variant="primary"
            :label="t('settings.sessionImportConfirmBtn')"
            :disabled="sessionImport.busy.value"
            @click="onRestoreConfirm"
          />
        </div>
      </div>
    </AppModal>
  </div>
</template>

<style scoped>
.font-select {
  min-width: 120px;
}
.chrome-dl {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
  min-width: 360px;
  max-width: 460px;
}
.chrome-dl__desc {
  margin: 0;
  font-size: var(--fs-sm);
  line-height: 1.6;
  color: var(--text-secondary);
}
.chrome-dl__err {
  color: var(--accent);
}
.chrome-dl__actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-3);
}
.settings-readonly {
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  color: var(--text-muted);
  word-break: break-all;
  text-align: right;
}
.settings-num {
  width: 110px;
}
.settings-input {
  width: 240px;
  max-width: 100%;
}
.settings-num,
.settings-input {
  height: 30px;
  padding: 0 var(--space-3);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-size: var(--fs-base);
  font-family: inherit;
}
.settings-num:focus,
.settings-input:focus {
  outline: none;
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}
/* Codex 导航行(整行可点 → 子页) */
.nav-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
  padding: var(--space-4);
  text-decoration: none;
  color: inherit;
  transition: background var(--transition);
}
.nav-row:hover {
  background: var(--surface-hover);
}
.nav-row__title {
  font-size: var(--fs-md);
  font-weight: 550;
  color: var(--text);
}
.nav-row__desc {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  margin-top: 2px;
  line-height: 1.4;
}
.nav-row__chevron {
  width: 16px;
  height: 16px;
  flex-shrink: 0;
  color: var(--text-muted);
}
</style>
