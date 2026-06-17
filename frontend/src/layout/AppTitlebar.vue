<script setup lang="ts">
import { computed } from 'vue'
import { useRoute } from 'vue-router'
import { i18nState, setLocale, t } from '@/i18n'
import ThemeSwitcher from '@/components/ui/ThemeSwitcher.vue'

const route = useRoute()
const currentTitle = computed(() => {
  const k = route.meta.navKey as string | undefined
  return k ? t(k) : ''
})
</script>

<template>
  <header class="titlebar">
    <h1 class="titlebar__title">{{ currentTitle }}</h1>

    <div class="titlebar__actions">
      <div class="lang-switch">
        <button :class="{ active: i18nState.locale === 'zh' }" @click="setLocale('zh')">中</button>
        <span class="lang-switch__sep">/</span>
        <button :class="{ active: i18nState.locale === 'en' }" @click="setLocale('en')">EN</button>
      </div>
      <ThemeSwitcher />
    </div>
  </header>
</template>

<style scoped>
.titlebar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  height: 52px;
  padding: 0 var(--space-5);
  border-bottom: 1px solid var(--border);
  background: color-mix(in srgb, var(--bg) 82%, transparent);
  backdrop-filter: blur(12px);
  -webkit-backdrop-filter: blur(12px);
}
.titlebar__title {
  font-size: var(--fs-lg);
  font-weight: 600;
}
.titlebar__actions {
  display: flex;
  align-items: center;
  gap: var(--space-4);
}
.lang-switch {
  display: flex;
  align-items: center;
  gap: var(--space-1);
  font-size: var(--fs-sm);
}
.lang-switch button {
  background: none;
  border: none;
  color: var(--text-muted);
  padding: 2px 4px;
  border-radius: var(--radius-sm);
  font-weight: 500;
}
.lang-switch button.active {
  color: var(--accent);
  font-weight: 600;
}
.lang-switch__sep {
  color: var(--border-strong);
}
</style>
