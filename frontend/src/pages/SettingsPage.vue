<script setup lang="ts">
import { ref } from 'vue'
import { i18nState, setLocale, t } from '@/i18n'
import { useAppearance, type Appearance } from '@/composables/useAppearance'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'

const { current, set } = useAppearance()

const themeOptions: { value: Appearance; label: string }[] = [
  { value: 'light', label: '白' },
  { value: 'dark', label: '黑' },
  { value: 'inkwash', label: '国风' },
]
const langOptions: { value: 'zh' | 'en'; label: string }[] = [
  { value: 'zh', label: '中文' },
  { value: 'en', label: 'EN' },
]

// 占位开关(Stage 3 接 settings store + /api/settings)
const autoApply = ref(true)
const autoUnlock = ref(false)
</script>

<template>
  <div>
    <SettingsGroup :title="t('nav.settings')">
      <SettingsRow title="外观" description="应用主题：白 / 黑 / 国风">
        <SegmentedControl
          :model-value="current"
          :options="themeOptions"
          @update:model-value="(v) => set(v as Appearance)"
        />
      </SettingsRow>
      <SettingsRow title="语言 / Language" description="界面显示语言">
        <SegmentedControl
          :model-value="i18nState.locale"
          :options="langOptions"
          @update:model-value="(v) => setLocale(v as 'zh' | 'en')"
        />
      </SettingsRow>
    </SettingsGroup>

    <SettingsGroup title="启动">
      <SettingsRow title="启动时自动应用" description="启动 transfer 时自动把当前提供商写入 Codex 配置">
        <AppSwitch v-model="autoApply" />
      </SettingsRow>
      <SettingsRow title="自动解锁 Codex 插件" description="无真实账号时通过 CDP 注入解锁(高延迟, 默认关)">
        <AppSwitch v-model="autoUnlock" />
      </SettingsRow>
    </SettingsGroup>
  </div>
</template>
