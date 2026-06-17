<script setup lang="ts">
import type { Component } from 'vue'

withDefaults(
  defineProps<{
    variant?: 'primary' | 'secondary' | 'ghost' | 'danger'
    size?: 'sm' | 'md'
    icon?: Component
    label?: string
    disabled?: boolean
  }>(),
  { variant: 'secondary', size: 'md' },
)
</script>

<template>
  <button
    type="button"
    class="app-btn"
    :class="[`app-btn--${variant}`, `app-btn--${size}`]"
    :disabled="disabled"
  >
    <component :is="icon" v-if="icon" class="app-btn__icon" />
    <span v-if="label || $slots.default" class="app-btn__label"><slot>{{ label }}</slot></span>
  </button>
</template>

<style scoped>
.app-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: var(--space-2);
  border: 1px solid transparent;
  border-radius: var(--radius);
  font-weight: 550;
  white-space: nowrap;
  transition: background var(--transition), border-color var(--transition), opacity var(--transition);
}
.app-btn:disabled {
  opacity: 0.5;
  cursor: default;
}
.app-btn--sm {
  height: 26px;
  padding: 0 var(--space-3);
  font-size: var(--fs-sm);
}
.app-btn--md {
  height: 32px;
  padding: 0 var(--space-4);
  font-size: var(--fs-base);
}
.app-btn__icon {
  width: 15px;
  height: 15px;
  flex-shrink: 0;
}
.app-btn--primary {
  background: var(--accent);
  color: var(--accent-text);
}
.app-btn--primary:hover:not(:disabled) {
  background: var(--accent-hover);
}
.app-btn--secondary {
  background: var(--surface);
  border-color: var(--border-strong);
  color: var(--text);
}
.app-btn--secondary:hover:not(:disabled) {
  background: var(--surface-hover);
}
.app-btn--ghost {
  background: transparent;
  color: var(--text-secondary);
}
.app-btn--ghost:hover:not(:disabled) {
  background: var(--surface-hover);
}
.app-btn--danger {
  background: transparent;
  border-color: var(--border-strong);
  color: var(--danger);
}
.app-btn--danger:hover:not(:disabled) {
  background: var(--danger-soft);
  border-color: var(--danger);
}
</style>
