<script setup lang="ts">
import { onMounted } from 'vue'
import AppLayout from './layout/AppLayout.vue'
import { useSettingsStore } from '@/stores/settings'
import { useAppearance } from '@/composables/useAppearance'
import { setLocale } from '@/i18n'

// 启动后从后端 /api/settings hydrate(权威源,覆盖 main.ts 的 localStorage 初值,跨设备一致)。
// load(false) 应用主题不回写、setLocale 仅本地不 PUT → 无 echo 回环。
const settings = useSettingsStore()
onMounted(async () => {
  const s = await settings.load().catch(() => null)
  if (!s) return
  if (typeof s.theme === 'string') useAppearance().load(s.theme)
  if (s.language === 'zh' || s.language === 'en') setLocale(s.language as 'zh' | 'en')
})
</script>

<template>
  <AppLayout />
</template>
