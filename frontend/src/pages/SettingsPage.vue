<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { i18nState, setLocale, t, tFmt } from '@/i18n'
import { useAppearance, type Appearance } from '@/composables/useAppearance'
import { useFont, type FontChoice, type FontSize } from '@/composables/useFont'
import { useSettingsStore } from '@/stores/settings'
import type { Settings } from '@/api/settings'
import { useToast } from '@/composables/useToast'
import { getAppVersion, checkAppUpdate, openExternalUrl } from '@/api/system'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'
import AppSelect from '@/components/ui/AppSelect.vue'
import AppButton from '@/components/ui/AppButton.vue'
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
const webFetchBackend = computed<string>({
  get: () => store.str('webFetchBackend', 'auto'),
  set: (v) => void persist({ webFetchBackend: v }),
})
const webFetchOptions: { value: string; label: string }[] = [
  { value: 'off', label: t('settings.webFetchBackend.off') },
  { value: 'auto', label: t('settings.webFetchBackend.auto') },
  { value: 'curl', label: 'curl' },
  { value: 'wreq', label: 'wreq' },
  { value: 'headless', label: 'headless' },
]

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
    </SettingsGroup>

    <SettingsGroup :title="t('settings.groupStartup')">
      <SettingsRow :title="t('settings.autoApplyOnStart')" :description="t('settings.autoApplyOnStartHint')">
        <AppSwitch v-model="autoApplyOnStart" />
      </SettingsRow>
      <SettingsRow :title="t('settings.restoreCodexOnExit')" :description="t('settings.restoreCodexOnExitHint')">
        <AppSwitch v-model="restoreCodexOnExit" />
      </SettingsRow>
      <SettingsRow :title="t('settings.autoUnlockCodexPlugins')" :description="t('settings.autoUnlockCodexPluginsHint')">
        <AppSwitch v-model="autoUnlockCodexPlugins" />
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
        <SegmentedControl v-model="webFetchBackend" :options="webFetchOptions" />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup :title="t('settings.groupCodexConfig')">
      <RouterLink to="/desktop" class="nav-row">
        <div class="nav-row__text">
          <div class="nav-row__title">{{ t('settings.codexCliRow') }}</div>
          <div class="nav-row__desc">{{ t('settings.codexCliRowDesc') }}</div>
        </div>
        <IconChevronRight class="nav-row__chevron" />
      </RouterLink>
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
        <AppButton size="sm" variant="ghost" :label="t('about.checkUpdate')" @click="onCheckUpdate" />
      </SettingsRow>
      <SettingsRow :title="t('settings.updateUrl')" :description="t('settings.updateUrlDesc')">
        <code class="settings-readonly">{{ UPDATE_REPO_URL }}</code>
      </SettingsRow>
      <SettingsRow :title="t('about.like')" :description="t('about.likeDesc')">
        <AppButton size="sm" variant="secondary" :label="t('about.like')" @click="openExternal(UPDATE_REPO_URL)" />
      </SettingsRow>
      <SettingsRow :title="t('about.feedback')" :description="t('about.feedbackDesc')">
        <AppButton size="sm" variant="ghost" :label="t('about.feedback')" @click="feedbackOpen = true" />
      </SettingsRow>
    </SettingsGroup>

    <FeedbackModal v-if="feedbackOpen" @close="feedbackOpen = false" />
  </div>
</template>

<style scoped>
.font-select {
  min-width: 120px;
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
