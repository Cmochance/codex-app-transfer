<script setup lang="ts">
import { computed, onMounted } from 'vue'
import { i18nState, setLocale, t } from '@/i18n'
import { useAppearance, type Appearance } from '@/composables/useAppearance'
import { useSettingsStore } from '@/stores/settings'
import type { Settings } from '@/api/settings'
import { useToast } from '@/composables/useToast'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'

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

// webFetchBackend(off/auto/curl/wreq/headless;仅 off/auto 有 i18n,余技术名)
// 默认 auto(对齐后端 schema DEFAULT_WEB_FETCH_BACKEND + 旧前端;key 缺失时运行时实为 auto)
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
    <h1 class="page-title">{{ t('settings.title') }}</h1>

    <SettingsGroup title="外观与语言">
      <SettingsRow :title="t('settings.theme')" description="应用主题:白 / 黑 / 国风">
        <SegmentedControl v-model="theme" :options="themeOptions" />
      </SettingsRow>
      <SettingsRow :title="t('settings.language')" description="界面显示语言">
        <SegmentedControl v-model="language" :options="langOptions" />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup title="启动与配置">
      <SettingsRow :title="t('settings.autoApplyOnStart')" :description="t('settings.autoApplyOnStartHint')">
        <AppSwitch v-model="autoApplyOnStart" />
      </SettingsRow>
      <SettingsRow :title="t('settings.restoreCodexOnExit')" :description="t('settings.restoreCodexOnExitHint')">
        <AppSwitch v-model="restoreCodexOnExit" />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup title="Codex 集成">
      <SettingsRow :title="t('settings.autoUnlockCodexPlugins')" :description="t('settings.autoUnlockCodexPluginsHint')">
        <AppSwitch v-model="autoUnlockCodexPlugins" />
      </SettingsRow>
      <SettingsRow :title="t('settings.autoWakeCodexPet')" :description="t('settings.autoWakeCodexPetHint')">
        <AppSwitch v-model="autoWakeCodexPet" />
      </SettingsRow>
      <SettingsRow :title="t('settings.codexQuotaEnabled')" :description="t('settings.codexQuotaEnabledHint')">
        <AppSwitch v-model="codexQuotaEnabled" />
      </SettingsRow>
      <SettingsRow :title="t('settings.codexNetworkAccess')" :description="t('settings.codexNetworkAccessHint')">
        <AppSwitch v-model="codexNetworkAccess" />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup title="提供商">
      <SettingsRow :title="t('settings.exposeAllModels')" description="OpenAI 模型菜单展示全部模型">
        <AppSwitch v-model="exposeAllProviderModels" />
      </SettingsRow>
      <SettingsRow :title="t('settings.showGrayProviders')" :description="t('settings.showGrayProvidersHint')">
        <AppSwitch v-model="showGrayProviders" />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup title="高级">
      <SettingsRow :title="t('settings.mcpCredentialsPortableStore')" :description="t('settings.mcpCredentialsPortableStoreHint')">
        <AppSwitch v-model="mcpCredentialsPortableStore" />
      </SettingsRow>
      <SettingsRow :title="t('settings.webFetchBackend')" :description="t('settings.webFetchBackendHint')">
        <SegmentedControl v-model="webFetchBackend" :options="webFetchOptions" />
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
    </SettingsGroup>
  </div>
</template>

<style scoped>
.page-title {
  font-size: var(--fs-xl);
  font-weight: 600;
  margin: 0 0 var(--space-5);
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
</style>
