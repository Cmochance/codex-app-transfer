<script setup lang="ts">
import { computed, onMounted } from 'vue'
import { i18nState, setLocale, t } from '@/i18n'
import { useAppearance, type Appearance } from '@/composables/useAppearance'
import { useFont, type FontChoice, type FontSize } from '@/composables/useFont'
import { useSettingsStore } from '@/stores/settings'
import type { Settings } from '@/api/settings'
import { useToast } from '@/composables/useToast'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'
import AppSelect from '@/components/ui/AppSelect.vue'
import ResidualScanPanel from '@/components/settings/ResidualScanPanel.vue'
import SnapshotPanel from '@/components/settings/SnapshotPanel.vue'
import DiagnosticPanel from '@/components/settings/DiagnosticPanel.vue'
import IconChevronRight from '~icons/lucide/chevron-right'

const store = useSettingsStore()
const { current: appearance, set: setAppearance } = useAppearance()
const { show: toast } = useToast()

onMounted(() => {
  if (!store.loaded) store.load().catch(() => {})
})

// 保存 partial → 后端浅合并;store.save 已做乐观更新 + 失败回滚,这里只 toast。
async function persist(partial: Settings) {
  try {
    const warn = await store.save(partial)
    if (warn) toast(warn, 'error')
  } catch (e) {
    toast((e as Error).message || '保存失败', 'error')
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

// theme / language 双向(同步本地状态 + 持久化服务端)
const theme = computed<Appearance>({
  get: () => appearance.value,
  set: (v) => {
    setAppearance(v)
    void persist({ theme: v })
  },
})
const language = computed<'zh' | 'en'>({
  get: () => i18nState.locale,
  set: (v) => {
    setLocale(v)
    void persist({ language: v })
  },
})
const themeOptions: { value: Appearance; label: string }[] = [
  { value: 'light', label: '白' },
  { value: 'dark', label: '黑' },
  { value: 'inkwash', label: '国风' },
]
const langOptions: { value: 'zh' | 'en'; label: string }[] = [
  { value: 'zh', label: '中文' },
  { value: 'en', label: 'EN' },
]

// 字体:按角色(正文/标题/等宽)+ 字号,纯 localStorage(useFont)。默认值 = 国风原字体。
const font = useFont()
const bodyFont = computed<FontChoice>({ get: () => font.body.value, set: (v) => font.setRole('body', v) })
const headingFont = computed<FontChoice>({
  get: () => font.heading.value,
  set: (v) => font.setRole('heading', v),
})
const monoFont = computed<FontChoice>({ get: () => font.mono.value, set: (v) => font.setRole('mono', v) })
const fontSize = computed<FontSize>({ get: () => font.size.value, set: (v) => font.setSize(v) })
const bodyFontOptions: { value: FontChoice; label: string }[] = [
  { value: 'system', label: '系统' },
  { value: 'songti', label: '宋体' },
  { value: 'kaiti', label: '楷体' },
  { value: 'rounded', label: '圆体' },
]
const headingFontOptions: { value: FontChoice; label: string }[] = [
  { value: 'songti', label: '宋体' },
  { value: 'kaiti', label: '楷体' },
  { value: 'system', label: '系统' },
]
const monoFontOptions: { value: FontChoice; label: string }[] = [
  { value: 'mono', label: '等宽' },
  { value: 'songti', label: '宋体' },
  { value: 'system', label: '系统' },
]
const fontSizeOptions: { value: FontSize; label: string }[] = [
  { value: 'small', label: '小' },
  { value: 'normal', label: '标准' },
  { value: 'large', label: '大' },
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
function onUpdateUrl(e: Event) {
  void persist({ updateUrl: (e.target as HTMLInputElement).value.trim() })
}
</script>

<template>
  <div>
    <SettingsGroup title="外观与语言">
      <SettingsRow :title="t('settings.theme')" description="应用主题:白 / 黑 / 国风">
        <SegmentedControl v-model="theme" :options="themeOptions" />
      </SettingsRow>
      <SettingsRow :title="t('settings.language')" description="界面显示语言">
        <SegmentedControl v-model="language" :options="langOptions" />
      </SettingsRow>
      <SettingsRow title="正文字体" description="界面正文字体(默认国风:系统)">
        <AppSelect v-model="bodyFont" :options="bodyFontOptions" class="font-select" />
      </SettingsRow>
      <SettingsRow title="标题字体" description="标题 / 分组名字体(默认国风:宋体)">
        <AppSelect v-model="headingFont" :options="headingFontOptions" class="font-select" />
      </SettingsRow>
      <SettingsRow title="等宽字体" description="代码 / JSON 等宽显示字体">
        <AppSelect v-model="monoFont" :options="monoFontOptions" class="font-select" />
      </SettingsRow>
      <SettingsRow title="字号" description="界面整体字号缩放">
        <SegmentedControl v-model="fontSize" :options="fontSizeOptions" />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup title="启动与配置">
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

    <SettingsGroup title="Codex 集成">
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

    <SettingsGroup title="Codex 配置">
      <RouterLink to="/desktop" class="nav-row">
        <div class="nav-row__text">
          <div class="nav-row__title">{{ t('desktop.title') }}</div>
          <div class="nav-row__desc">{{ t('desktop.subtitle') }}</div>
        </div>
        <IconChevronRight class="nav-row__chevron" />
      </RouterLink>
      <ResidualScanPanel />
      <SnapshotPanel />
    </SettingsGroup>

    <SettingsGroup title="高级">
      <SettingsRow :title="t('settings.exposeAllModels')" description="OpenAI 模型菜单展示全部模型">
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
      <SettingsRow :title="t('settings.proxyPort')" description="本地转发代理监听端口(改后需重启生效)">
        <input
          type="number"
          class="settings-num"
          :value="store.num('proxyPort', 0) || ''"
          min="1"
          max="65535"
          @change="onPort('proxyPort', $event)"
        />
      </SettingsRow>
      <SettingsRow :title="t('settings.adminPort')" description="管理 API 端口(改后需重启生效)">
        <input
          type="number"
          class="settings-num"
          :value="store.num('adminPort', 0) || ''"
          min="1"
          max="65535"
          @change="onPort('adminPort', $event)"
        />
      </SettingsRow>
      <SettingsRow :title="t('settings.updateUrl')" description="自定义更新检查地址(留空用默认)">
        <input
          type="text"
          class="settings-input"
          :value="store.str('updateUrl')"
          placeholder="https://..."
          spellcheck="false"
          @change="onUpdateUrl"
        />
      </SettingsRow>
      <DiagnosticPanel />
    </SettingsGroup>
  </div>
</template>

<style scoped>
.font-select {
  min-width: 120px;
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
