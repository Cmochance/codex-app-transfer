<script setup lang="ts">
import type { Component } from 'vue'
import { t } from '@/i18n'
import IconGauge from '~icons/lucide/gauge'
import IconPlug from '~icons/lucide/plug'
import IconRadio from '~icons/lucide/radio'
import IconChart from '~icons/lucide/bar-chart-3'
import IconSettings from '~icons/lucide/settings'
import IconBookmark from '~icons/lucide/bookmark'
import IconPalette from '~icons/lucide/palette'
import IconBook from '~icons/lucide/book-open'
import IconLayers from '~icons/lucide/layers'

interface NavItem {
  to: string
  key: string
  icon: Component
}

const mainNav: NavItem[] = [
  { to: '/dashboard', key: 'nav.dashboard', icon: IconGauge },
  { to: '/providers', key: 'nav.providers', icon: IconPlug },
  { to: '/proxy', key: 'nav.proxy', icon: IconRadio },
  { to: '/usage', key: 'nav.usage', icon: IconChart },
]
const codexNav: NavItem[] = [
  { to: '/codex', key: 'nav.codex', icon: IconBookmark },
  { to: '/codex-skin', key: 'nav.theme', icon: IconPalette },
]
const bottomNav: NavItem[] = [
  { to: '/settings', key: 'nav.settings', icon: IconSettings },
  { to: '/guide', key: 'nav.guide', icon: IconBook },
]
</script>

<template>
  <aside class="sidebar">
    <div class="sidebar__brand">
      <span class="sidebar__logo"><IconLayers /></span>
      <span class="sidebar__title">Codex Transfer</span>
    </div>

    <nav class="sidebar__nav">
      <RouterLink v-for="item in mainNav" :key="item.to" :to="item.to" class="nav-item" active-class="nav-item--active">
        <component :is="item.icon" class="nav-item__icon" />
        <span>{{ t(item.key) }}</span>
      </RouterLink>

      <div class="sidebar__sep" />

      <RouterLink v-for="item in codexNav" :key="item.to" :to="item.to" class="nav-item" active-class="nav-item--active">
        <component :is="item.icon" class="nav-item__icon" />
        <span>{{ t(item.key) }}</span>
      </RouterLink>
    </nav>

    <nav class="sidebar__bottom">
      <RouterLink v-for="item in bottomNav" :key="item.to" :to="item.to" class="nav-item" active-class="nav-item--active">
        <component :is="item.icon" class="nav-item__icon" />
        <span>{{ t(item.key) }}</span>
      </RouterLink>
    </nav>
  </aside>
</template>

<style scoped>
.sidebar {
  display: flex;
  flex-direction: column;
  height: 100vh;
  padding: var(--space-3) var(--space-2);
  background: var(--sidebar-bg);
  backdrop-filter: blur(24px) saturate(180%);
  -webkit-backdrop-filter: blur(24px) saturate(180%);
  border-right: 1px solid var(--border);
}
.sidebar__brand {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  padding: var(--space-2) var(--space-2) var(--space-4);
}
.sidebar__logo {
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  border-radius: var(--radius-sm);
  background: var(--accent);
  color: var(--accent-text);
  font-size: 15px;
}
.sidebar__title {
  font-size: var(--fs-md);
  font-weight: 600;
  letter-spacing: -0.01em;
}
.sidebar__nav {
  display: flex;
  flex-direction: column;
  gap: 2px;
  flex: 1;
}
.sidebar__bottom {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding-top: var(--space-2);
}
.sidebar__sep {
  height: 1px;
  background: var(--border);
  margin: var(--space-3) var(--space-2);
}
.nav-item {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  height: 32px;
  padding: 0 var(--space-3);
  border-radius: var(--radius-sm);
  color: var(--text-secondary);
  font-size: var(--fs-base);
  font-weight: 500;
  text-decoration: none;
  transition: background var(--transition), color var(--transition);
}
.nav-item:hover {
  background: var(--surface-hover);
  text-decoration: none;
}
.nav-item--active {
  background: var(--accent-soft);
  color: var(--accent);
  font-weight: 600;
}
.nav-item__icon {
  width: 17px;
  height: 17px;
  flex-shrink: 0;
}
</style>
