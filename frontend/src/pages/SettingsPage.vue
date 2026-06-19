<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { i18nState, setLocale, t, tFmt } from '@/i18n'
import { useAppearance, type Appearance } from '@/composables/useAppearance'
import { useFont, type FontChoice, type FontSize } from '@/composables/useFont'
import { useSettingsStore } from '@/stores/settings'
import type { Settings } from '@/api/settings'
import { useToast } from '@/composables/useToast'
import { getAppVersion, checkAppUpdate, openExternalUrl } from '@/api/system'
import {
  pluginUnlockStatus,
  pluginUnlockStart,
  pluginUnlockReinject,
  type PluginUnlockState,
} from '@/api/desktop'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'
import AppSelect from '@/components/ui/AppSelect.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppModal from '@/components/ui/AppModal.vue'
import { getChromeReady, ensureChrome, getSystemProxyStatus, type SystemProxyStatus } from '@/api/chrome'
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

onMounted(() => {
  if (!store.loaded) store.load().catch(() => {})
  getAppVersion()
    .then((r) => (appVersion.value = r.version || ''))
    .catch(() => {})
})

// 关于:检查更新 + 外链(走系统浏览器)
async function onCheckUpdate() {
  try {
    const r = await checkAppUpdate()
    const latest = r.latestVersion
    if (r.hasUpdate || (latest && latest !== appVersion.value)) {
      toast(tFmt('about.updateAvailable', { version: latest || '' }))
    } else {
      toast(t('about.upToDate'))
    }
  } catch (e) {
    toast((e as Error).message || t('about.checkFailed'), 'error')
  }
}
function openExternal(url: string) {
  openExternalUrl(url).catch((e) => toast((e as Error).message, 'error'))
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
const autoUnlockCodexPlugins = toggle('autoUnlockCodexPlugins', false)
const autoWakeCodexPet = toggle('autoWakeCodexPet', true)
const codexQuotaEnabled = toggle('codexQuotaEnabled', false)
const codexNetworkAccess = toggle('codexNetworkAccess', false)
const exposeAllProviderModels = toggle('exposeAllProviderModels', false)
const showGrayProviders = toggle('showGrayProviders', false)
const mcpCredentialsPortableStore = toggle('mcpCredentialsPortableStore', true)
const hideDockIcon = toggle('hideDockIcon', false)
// macOS 限定:隐藏程序坞图标(Windows/Linux 无 Dock 概念,该开关不显示)
const isMac = typeof navigator !== 'undefined' && /Mac/i.test(navigator.userAgent)

// ── Codex 插件解锁 daemon:开关开启时轮询运行时状态 + 强制开启(手动注入)──
const unlockState = ref<PluginUnlockState | ''>('')
const unlockForcing = ref(false)
const unlockStateLabel = computed(() =>
  unlockState.value ? t(`settings.pluginUnlockState.${unlockState.value}`) : '',
)
let unlockTimer: ReturnType<typeof setInterval> | null = null
async function refreshUnlockState() {
  try {
    unlockState.value = (await pluginUnlockStatus()).status
  } catch {
    unlockState.value = ''
  }
}
function stopUnlockPoll() {
  if (unlockTimer) {
    clearInterval(unlockTimer)
    unlockTimer = null
  }
}
function startUnlockPoll() {
  stopUnlockPoll()
  refreshUnlockState()
  unlockTimer = setInterval(refreshUnlockState, 3000)
}
// 开关开 → 轮询;关 → 停轮询并清状态(daemon 已被后端 stop)
watch(
  autoUnlockCodexPlugins,
  (on) => {
    if (on) startUnlockPoll()
    else {
      stopUnlockPoll()
      unlockState.value = ''
    }
  },
  { immediate: true },
)
onUnmounted(stopUnlockPoll)
// 强制开启:先确保 daemon 在跑(/start 幂等),再触发一次注入(/reinject)。
async function forceUnlock() {
  unlockForcing.value = true
  try {
    await pluginUnlockStart()
    await pluginUnlockReinject()
    toast(t('settings.pluginUnlockForced'))
  } catch (e) {
    toast((e as Error).message || t('settings.pluginUnlockForceFailed'), 'error')
  } finally {
    unlockForcing.value = false
    refreshUnlockState()
  }
}

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
      <SettingsRow :title="t('settings.autoUnlockCodexPlugins')" :description="t('settings.autoUnlockCodexPluginsHint')">
        <div class="unlock-ctl">
          <template v-if="autoUnlockCodexPlugins">
            <span
              v-if="unlockStateLabel"
              class="unlock-ctl__state"
              :class="`is-${unlockState}`"
              >{{ unlockStateLabel }}</span
            >
            <AppButton
              size="sm"
              variant="secondary"
              :label="t('settings.pluginUnlockForce')"
              :disabled="unlockForcing"
              @click="forceUnlock"
            />
          </template>
          <AppSwitch v-model="autoUnlockCodexPlugins" />
        </div>
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
      <SnapshotPanel />
    </SettingsGroup>

    <SettingsGroup :title="t('settings.groupAdvanced')">
      <SettingsRow :title="t('settings.exposeAllModels')" :description="t('settings.exposeAllModelsDesc')">
        <AppSwitch v-model="exposeAllProviderModels" />
      </SettingsRow>
      <SettingsRow :title="t('settings.showGrayProviders')" :description="t('settings.showGrayProvidersHint')">
        <AppSwitch v-model="showGrayProviders" />
      </SettingsRow>
      <SettingsRow
        :title="t('settings.mcpCredentialsPortableStore')"
        :description="t('settings.mcpCredentialsPortableStoreHint')"
      >
        <AppSwitch v-model="mcpCredentialsPortableStore" />
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
.unlock-ctl {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.unlock-ctl__state {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  white-space: nowrap;
}
.unlock-ctl__state.is-injected {
  color: var(--success);
}
.unlock-ctl__state.is-failed {
  color: var(--danger);
}
</style>
