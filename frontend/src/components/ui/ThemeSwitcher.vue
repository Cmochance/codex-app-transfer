<script setup lang="ts">
import type { Component } from 'vue'
import { useAppearance, type Appearance } from '@/composables/useAppearance'
import IconSun from '~icons/lucide/sun'
import IconMoon from '~icons/lucide/moon'
import IconBrush from '~icons/lucide/brush'

const { current, set } = useAppearance()

interface ThemeOption {
  key: Appearance
  icon: Component
  label: string
}
const themes: ThemeOption[] = [
  { key: 'light', icon: IconSun, label: '白' },
  { key: 'dark', icon: IconMoon, label: '黑' },
  { key: 'inkwash', icon: IconBrush, label: '米' },
]
</script>

<template>
  <div class="theme-switcher" role="group" aria-label="主题切换">
    <button
      v-for="th in themes"
      :key="th.key"
      type="button"
      class="theme-switcher__btn"
      :class="{ active: current === th.key }"
      :title="th.label"
      :aria-pressed="current === th.key"
      @click="set(th.key)"
    >
      <component :is="th.icon" />
    </button>
  </div>
</template>

<style scoped>
.theme-switcher {
  display: inline-flex;
  padding: 2px;
  gap: 2px;
  background: var(--surface-2);
  border: 1px solid var(--border);
  border-radius: var(--radius);
}
.theme-switcher__btn {
  display: grid;
  place-items: center;
  width: 28px;
  height: 24px;
  border: none;
  background: transparent;
  color: var(--text-muted);
  border-radius: var(--radius-sm);
  transition: background var(--transition), color var(--transition);
}
.theme-switcher__btn:hover {
  color: var(--text);
}
.theme-switcher__btn.active {
  background: var(--surface);
  color: var(--accent);
  box-shadow: var(--shadow-sm);
}
.theme-switcher__btn :deep(svg) {
  width: 15px;
  height: 15px;
}
</style>
