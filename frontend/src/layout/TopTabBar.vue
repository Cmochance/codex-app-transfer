<script setup lang="ts">
import type { Component } from 'vue'
import { t } from '@/i18n'
import IconPlug from '~icons/lucide/plug'
import IconRadio from '~icons/lucide/radio'
import IconChart from '~icons/lucide/bar-chart-3'
import IconBookmark from '~icons/lucide/bookmark'
import IconSettings from '~icons/lucide/settings'

interface Tab {
  to: string
  key: string
  icon: Component
}
// FineTune 风顶部居中 tab(图标在上 + 文字在下)。次要页(guide/codex-skin/desktop/
// provider-form)经页面内链接 / 子路由进, 不占顶部 tab。
const tabs: Tab[] = [
  { to: '/providers', key: 'nav.providers', icon: IconPlug },
  { to: '/proxy', key: 'nav.proxy', icon: IconRadio },
  { to: '/usage', key: 'nav.usage', icon: IconChart },
  { to: '/codex', key: 'nav.codex', icon: IconBookmark },
  { to: '/settings', key: 'nav.settings', icon: IconSettings },
]
</script>

<template>
  <header class="topbar">
    <div class="topbar__title">Codex App Transfer</div>
    <nav class="tabbar">
      <RouterLink
        v-for="tab in tabs"
        :key="tab.to"
        :to="tab.to"
        class="tab"
        active-class="tab--active"
      >
        <component :is="tab.icon" class="tab__icon" />
        <span class="tab__label">{{ t(tab.key) }}</span>
      </RouterLink>
    </nav>
  </header>
</template>

<style scoped>
.topbar {
  flex-shrink: 0;
  padding: var(--space-3) 0 var(--space-2);
  border-bottom: 1px solid var(--border);
  background: color-mix(in srgb, var(--bg) 88%, transparent);
  backdrop-filter: blur(16px);
  -webkit-backdrop-filter: blur(16px);
}
.topbar__title {
  text-align: center;
  font-size: var(--fs-md);
  font-weight: 600;
  margin-bottom: var(--space-3);
  letter-spacing: -0.01em;
}
.tabbar {
  display: flex;
  justify-content: center;
  gap: var(--space-1);
}
.tab {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  width: 76px;
  padding: var(--space-2) var(--space-1) 6px;
  border-radius: var(--radius);
  color: var(--text-secondary);
  text-decoration: none;
  transition: background var(--transition), color var(--transition);
}
.tab:hover {
  background: var(--surface-hover);
  text-decoration: none;
}
.tab--active {
  background: var(--surface-2);
  color: var(--accent);
}
.tab__icon {
  width: 20px;
  height: 20px;
}
.tab__label {
  font-size: var(--fs-sm);
  font-weight: 500;
}
</style>
